//! `loom-daemon clean`: the native port of `loom-clean` (`clean.py`).
//!
//! Covers standard + `--safe` worktree cleanup, local-branch cleanup
//! (two-pass: remote-ref-gone, then issue-state), tmux session cleanup,
//! per-agent Claude config-dir cleanup, `--deep` build-artifact cleanup,
//! and `--daemon` crash recovery (stale `loom:building` label revert +
//! stale spawn-loop claim-lock cleanup). `--aggressive` lives in
//! `aggressive.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};

use super::gh;
use super::liveness::active_spawn_loop_issues;
use super::naming::{self, BRANCH_PREFIX};
use super::safety::{
    check_uncommitted_changes, find_processes_using_directory, read_in_use_marker,
};

/// Default grace period after PR merge before a worktree is eligible for
/// `--safe` removal (10 minutes).
pub const DEFAULT_GRACE_PERIOD_SECS: i64 = 600;

/// Minimum age before a `.loom/sweep-checkpoint/` transient is eligible for
/// bulk pruning (48 hours). Belt-and-suspenders on top of the liveness checks
/// in [`clean_sweep_transients`]: a sweep that has only just started (its
/// registry write racing this scan) is never touched, and it bounds the number
/// of forge probes a single clean pass can issue.
pub const SWEEP_TRANSIENT_MIN_AGE_SECS: u64 = 48 * 60 * 60;

/// Prompt on stdout/stdin for a `[y/N]` confirmation. EOF (no TTY attached,
/// e.g. under cron) is treated as "no" — matches
/// `clean.py`'s `except (EOFError, KeyboardInterrupt): response = ""` fallback.
fn confirm(prompt: &str) -> bool {
    use std::io::Write;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

#[derive(Debug, Default)]
pub struct CleanupStats {
    pub cleaned_worktrees: usize,
    pub skipped_open: usize,
    pub skipped_in_use: usize,
    pub skipped_not_merged: usize,
    pub skipped_grace: usize,
    pub skipped_uncommitted: usize,
    pub skipped_editable: usize,
    pub cleaned_branches: usize,
    pub kept_branches: usize,
    pub errored_branches: usize,
    pub killed_tmux: usize,
    pub cleaned_config_dirs: usize,
    pub cleaned_sweep_baselines: usize,
    pub cleaned_sweep_checkpoints: usize,
    pub kept_sweep_transients: usize,
    pub errors: usize,
}

/// Options mirroring `clean.py`'s argparse surface (minus subcommand-style
/// flags handled by the CLI layer directly: `--aggressive` routes to
/// `aggressive::clean_aggressive`, `--daemon` to [`clean_daemon_crash_state`]).
#[derive(Debug, Clone)]
pub struct CleanOptions {
    pub dry_run: bool,
    pub deep: bool,
    pub force: bool,
    pub safe: bool,
    pub grace_period_secs: i64,
    pub worktrees_only: bool,
    pub branches_only: bool,
    pub tmux_only: bool,
}

impl Default for CleanOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            deep: false,
            force: false,
            safe: false,
            grace_period_secs: DEFAULT_GRACE_PERIOD_SECS,
            worktrees_only: false,
            branches_only: false,
            tmux_only: false,
        }
    }
}

/// PR status for a closed issue's worktree, used by `--safe` mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrStatus {
    Merged { merged_at: String },
    ClosedNoMerge,
    Open,
    NoPr,
    Unknown,
}

#[derive(serde::Deserialize)]
struct PrRow {
    state: String,
    #[serde(default, rename = "mergedAt")]
    merged_at: Option<String>,
}

fn gh_pr_list(repo_root: &Path, args: &[&str]) -> Option<Vec<PrRow>> {
    let out = Command::new("gh")
        .args(args)
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Check the PR status for `issue_num`'s branch. Thin `gh` wrapper mirroring
/// `clean.py::check_pr_merged`.
#[must_use]
pub fn check_pr_merged(repo_root: &Path, issue_num: u32) -> PrStatus {
    let branch = naming::branch_name(issue_num);
    let rows = gh_pr_list(
        repo_root,
        &[
            "pr",
            "list",
            "--head",
            &branch,
            "--state",
            "all",
            "--json",
            "number,state,mergedAt",
            "--limit",
            "1",
        ],
    )
    .or_else(|| {
        gh_pr_list(
            repo_root,
            &[
                "pr",
                "list",
                "--search",
                &format!("Closes #{issue_num}"),
                "--state",
                "all",
                "--json",
                "number,state,mergedAt",
                "--limit",
                "1",
            ],
        )
    });
    let Some(rows) = rows else {
        return PrStatus::Unknown;
    };
    let Some(row) = rows.into_iter().next() else {
        return PrStatus::NoPr;
    };
    if let Some(merged_at) = row.merged_at {
        PrStatus::Merged { merged_at }
    } else if row.state == "CLOSED" {
        PrStatus::ClosedNoMerge
    } else if row.state == "OPEN" {
        PrStatus::Open
    } else {
        PrStatus::Unknown
    }
}

/// Whether the grace period since `merged_at` has passed. Pure and
/// unit-testable — mirrors `clean.py::check_grace_period`.
#[must_use]
pub fn check_grace_period(
    merged_at: DateTime<Utc>,
    grace_period_secs: i64,
    now: DateTime<Utc>,
) -> (bool, i64) {
    let elapsed = now.signed_duration_since(merged_at).num_seconds();
    if elapsed > grace_period_secs {
        (true, 0)
    } else {
        (false, grace_period_secs - elapsed)
    }
}

/// Find pip packages with editable installs pointing into `worktree_path`.
/// Best-effort; mirrors `clean.py::find_editable_pip_installs` closely enough
/// to preserve the safety gate (skip removal when present, unless `--force`).
#[must_use]
pub fn find_editable_pip_installs(worktree_path: &Path) -> Vec<String> {
    let worktree_str = worktree_path
        .canonicalize()
        .unwrap_or_else(|_| worktree_path.to_path_buf());
    let worktree_str = worktree_str.to_string_lossy().to_string();

    let mut interpreters: Vec<String> = Vec::new();
    for name in ["python3", "python"] {
        if let Ok(out) = Command::new("which").arg(name).output() {
            if out.status.success() {
                let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !path.is_empty() && !interpreters.contains(&path) {
                    interpreters.push(path);
                }
            }
        }
    }
    for candidate in [worktree_path.join(".venv").join("bin").join("python")] {
        if candidate.is_file() {
            let s = candidate.to_string_lossy().to_string();
            if !interpreters.contains(&s) {
                interpreters.push(s);
            }
        }
    }

    let mut packages: Vec<String> = Vec::new();
    for interpreter in &interpreters {
        let Ok(out) = Command::new(interpreter)
            .args(["-m", "pip", "list", "--editable", "--format=json"])
            .output()
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let Ok(list) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) else {
            continue;
        };
        for pkg in list {
            let Some(name) = pkg.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let Ok(show) = Command::new(interpreter)
                .args(["-m", "pip", "show", name])
                .output()
            else {
                continue;
            };
            if !show.status.success() {
                continue;
            }
            let show_text = String::from_utf8_lossy(&show.stdout);
            for line in show_text.lines() {
                if let Some(loc) = line
                    .strip_prefix("Editable project location:")
                    .or_else(|| line.strip_prefix("Location:"))
                {
                    let loc = loc.trim();
                    if loc.starts_with(&worktree_str) && !packages.iter().any(|p| p == name) {
                        packages.push(name.to_string());
                    }
                    break;
                }
            }
        }
    }
    packages
}

fn cleanup_worktree(repo_root: &Path, worktree_path: &Path, issue_num: u32, dry_run: bool) -> bool {
    let branch_name = naming::branch_name(issue_num);
    if dry_run {
        println!("Would remove: {}", worktree_path.display());
        println!("Would delete branch: {branch_name}");
        return true;
    }
    let removed = Command::new("git")
        .args(["worktree", "remove"])
        .arg(worktree_path)
        .arg("--force")
        .current_dir(repo_root)
        .status()
        .is_ok_and(|s| s.success());
    if !removed {
        eprintln!("  Failed to remove worktree: {}", worktree_path.display());
        return false;
    }
    println!("  Removed worktree: {}", worktree_path.display());

    let deleted = Command::new("git")
        .args(["branch", "-d", &branch_name])
        .current_dir(repo_root)
        .status()
        .is_ok_and(|s| s.success());
    if !deleted {
        let _ = Command::new("git")
            .args(["branch", "-D", &branch_name])
            .current_dir(repo_root)
            .status();
    }
    true
}

/// Standard + `--safe` worktree cleanup pass. Mirrors `clean.py::clean_worktrees`.
pub fn clean_worktrees(repo_root: &Path, stats: &mut CleanupStats, opts: &CleanOptions) {
    let worktrees_dir = crate::worktree_root::worktree_root(repo_root);
    if !worktrees_dir.is_dir() {
        println!("No worktrees directory found");
        return;
    }

    let active_issues = active_spawn_loop_issues(repo_root);

    let Ok(entries) = std::fs::read_dir(&worktrees_dir) else {
        return;
    };
    let mut worktree_dirs: Vec<_> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(naming::WORKTREE_PREFIX)
        })
        .collect();
    worktree_dirs.sort_by_key(std::fs::DirEntry::path);

    for entry in worktree_dirs {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(issue_num) = naming::issue_from_worktree(&name) else {
            continue;
        };
        let worktree_path = entry.path().canonicalize().unwrap_or_else(|_| entry.path());

        println!("Checking worktree: issue-{issue_num}");

        if !opts.force && active_issues.contains(&issue_num) {
            println!("  Issue #{issue_num} has a live spawn-loop task or claim-lock - preserving");
            stats.skipped_in_use += 1;
            continue;
        }

        if let Some(marker) = read_in_use_marker(&worktree_path) {
            println!(
                "  Worktree in use by shepherd (task: {}, pid: {}) - preserving",
                marker.task_id, marker.pid
            );
            stats.skipped_in_use += 1;
            continue;
        }

        if !opts.force {
            let active_pids = find_processes_using_directory(&worktree_path);
            if !active_pids.is_empty() {
                println!("  Active process(es) using worktree: {active_pids:?} - preserving");
                stats.skipped_in_use += 1;
                continue;
            }
        }

        let editable_pkgs = find_editable_pip_installs(&worktree_path);
        if !editable_pkgs.is_empty() {
            let pkg_list = editable_pkgs.join(", ");
            if opts.force {
                println!(
                    "  Editable pip install(s) found ({pkg_list}) - removing anyway (--force)"
                );
            } else {
                println!("  Editable pip install(s) found ({pkg_list}) - skipping");
                stats.skipped_editable += 1;
                continue;
            }
        }

        let issue_state = gh::issue_state(repo_root, issue_num);
        if issue_state != "CLOSED" {
            println!("  Issue #{issue_num} is {issue_state} - preserving");
            stats.skipped_open += 1;
            continue;
        }

        if opts.safe {
            let pr_status = check_pr_merged(repo_root, issue_num);
            match pr_status {
                PrStatus::Merged { merged_at } => {
                    if !opts.force {
                        if let Ok(dt) = DateTime::parse_from_rfc3339(&merged_at) {
                            let (passed, remaining) = check_grace_period(
                                dt.with_timezone(&Utc),
                                opts.grace_period_secs,
                                Utc::now(),
                            );
                            if !passed {
                                println!("  PR merged but grace period not passed ({remaining}s remaining)");
                                stats.skipped_grace += 1;
                                continue;
                            }
                        }
                        if check_uncommitted_changes(&worktree_path) {
                            println!("  Uncommitted changes detected - skipping");
                            stats.skipped_uncommitted += 1;
                            continue;
                        }
                    }
                    if cleanup_worktree(repo_root, &worktree_path, issue_num, opts.dry_run) {
                        stats.cleaned_worktrees += 1;
                    } else {
                        stats.errors += 1;
                    }
                }
                PrStatus::ClosedNoMerge => {
                    println!("  PR closed without merge - skipping (may need investigation)");
                    stats.skipped_not_merged += 1;
                }
                PrStatus::Open => {
                    println!("  PR still open - skipping");
                    stats.skipped_open += 1;
                }
                PrStatus::NoPr => {
                    println!("  No PR found for closed issue - skipping");
                    stats.skipped_not_merged += 1;
                }
                PrStatus::Unknown => {
                    println!("  Unknown PR status - skipping");
                    stats.errors += 1;
                }
            }
        } else {
            println!("  Issue #{issue_num} is CLOSED");
            if opts.dry_run {
                println!("  Would remove: {}", entry.path().display());
                stats.cleaned_worktrees += 1;
            } else if opts.force {
                println!("  Auto-removing: {}", entry.path().display());
                if cleanup_worktree(repo_root, &worktree_path, issue_num, opts.dry_run) {
                    stats.cleaned_worktrees += 1;
                } else {
                    stats.errors += 1;
                }
            } else if confirm("  Force remove this worktree? [y/N] ") {
                if cleanup_worktree(repo_root, &worktree_path, issue_num, opts.dry_run) {
                    stats.cleaned_worktrees += 1;
                } else {
                    stats.errors += 1;
                }
            } else {
                println!("  Skipping: {}", entry.path().display());
                stats.skipped_open += 1;
            }
        }
    }
}

pub fn prune_orphaned_worktrees(repo_root: &Path, dry_run: bool) {
    println!("\nPruning Orphaned References");
    let mut args = vec!["worktree", "prune"];
    if dry_run {
        args.push("--dry-run");
    }
    args.push("--verbose");
    let out = Command::new("git")
        .args(&args)
        .current_dir(repo_root)
        .output();
    match out {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if !stdout.trim().is_empty() {
                println!("{}", stdout.trim());
            } else {
                println!("No orphaned worktrees to prune");
            }
        }
        Err(e) => eprintln!("Error pruning worktrees: {e}"),
    }
}

fn current_branch(repo_root: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn checked_out_branches(repo_root: &Path) -> std::collections::HashSet<String> {
    let mut out_set = std::collections::HashSet::new();
    let Ok(out) = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output()
    else {
        return out_set;
    };
    if !out.status.success() {
        return out_set;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if let Some(name) = line.strip_prefix("branch refs/heads/") {
            out_set.insert(name.trim().to_string());
        }
    }
    out_set
}

fn default_branch(repo_root: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let ref_name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    ref_name
        .strip_prefix("refs/remotes/origin/")
        .map(str::to_string)
}

fn remote_branch_exists(repo_root: &Path, branch: &str) -> bool {
    Command::new("git")
        .args(["show-ref", "--verify", "--quiet", &format!("refs/remotes/origin/{branch}")])
        .current_dir(repo_root)
        .status()
        .map(|s| s.success())
        // Fail closed: if we can't probe, claim the remote exists so we
        // don't delete a branch on a transient git error.
        .unwrap_or(true)
}

/// Two-pass local-branch cleanup. Mirrors `clean.py::clean_branches`.
pub fn clean_branches(repo_root: &Path, stats: &mut CleanupStats, dry_run: bool) {
    let out = Command::new("git")
        .args(["branch", "--format=%(refname:short)"])
        .current_dir(repo_root)
        .output();
    let branches: Vec<String> = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    };
    if branches.is_empty() {
        println!("No local branches found");
        return;
    }

    let mut protected: std::collections::HashSet<String> =
        std::collections::HashSet::from(["main".to_string()]);
    if let Some(d) = default_branch(repo_root) {
        protected.insert(d);
    }
    if let Some(c) = current_branch(repo_root) {
        protected.insert(c);
    }
    protected.extend(checked_out_branches(repo_root));

    let mut issue_pass_candidates: Vec<String> = Vec::new();
    for branch in &branches {
        if protected.contains(branch) {
            continue;
        }
        if !remote_branch_exists(repo_root, branch) {
            println!("  Stale (no origin/{branch}) - deleting {branch}");
            if dry_run {
                stats.cleaned_branches += 1;
                continue;
            }
            let ok = Command::new("git")
                .args(["branch", "-D", branch])
                .current_dir(repo_root)
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                stats.cleaned_branches += 1;
            } else {
                stats.errors += 1;
            }
        } else {
            issue_pass_candidates.push(branch.clone());
        }
    }

    for branch in &issue_pass_candidates {
        let Some(rest) = branch.strip_prefix(BRANCH_PREFIX) else {
            continue;
        };
        let Ok(issue_num) = rest.parse::<u32>() else {
            continue;
        };

        let status = gh::issue_state(repo_root, issue_num);
        match status.as_str() {
            "CLOSED" => {
                println!("  Issue #{issue_num} CLOSED - deleting {branch}");
                if !dry_run {
                    let ok = Command::new("git")
                        .args(["branch", "-D", branch])
                        .current_dir(repo_root)
                        .status()
                        .is_ok_and(|s| s.success());
                    if ok {
                        stats.cleaned_branches += 1;
                    } else {
                        stats.errors += 1;
                    }
                } else {
                    stats.cleaned_branches += 1;
                }
            }
            "OPEN" => {
                println!("  Issue #{issue_num} OPEN - keeping {branch}");
                stats.kept_branches += 1;
            }
            _ => {
                eprintln!("  Could not probe issue #{issue_num} for {branch}: gh lookup returned {status}");
                stats.errored_branches += 1;
            }
        }
    }
}

fn list_loom_tmux_sessions() -> Vec<String> {
    let Ok(out) = Command::new("tmux")
        .args(["-L", "loom", "list-sessions", "-F", "#{session_name}"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn clean_tmux_sessions(stats: &mut CleanupStats, dry_run: bool) {
    let sessions = list_loom_tmux_sessions();
    if sessions.is_empty() {
        println!("No Loom tmux sessions found");
        return;
    }
    println!("Found Loom tmux sessions:");
    for s in &sessions {
        println!("  - {s}");
    }
    println!();
    if dry_run {
        println!("Would kill these sessions");
        stats.killed_tmux = sessions.len();
    } else {
        for s in &sessions {
            let ok = Command::new("tmux")
                .args(["-L", "loom", "kill-session", "-t", s])
                .status()
                .is_ok_and(|st| st.success());
            if ok {
                println!("Killed: {s}");
                stats.killed_tmux += 1;
            }
        }
    }
}

pub fn clean_agent_config(repo_root: &Path, stats: &mut CleanupStats, dry_run: bool) {
    let base_dir = repo_root.join(".loom").join("claude-config");
    if !base_dir.is_dir() {
        println!("No agent config directories found");
        return;
    }
    let Ok(entries) = std::fs::read_dir(&base_dir) else {
        println!("No agent config directories found");
        return;
    };
    let dirs: Vec<_> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .collect();
    if dirs.is_empty() {
        println!("No agent config directories found");
        return;
    }
    if dry_run {
        println!("Would remove {} agent config dir(s) from {}", dirs.len(), base_dir.display());
        stats.cleaned_config_dirs = dirs.len();
        return;
    }
    let mut removed = 0usize;
    for entry in dirs {
        if std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    println!("Removed {removed} agent config dir(s)");
    stats.cleaned_config_dirs = removed;
}

/// `<repo_root>/.loom/sweep-checkpoint/` — where `/loom:sweep` keeps its
/// per-issue checkpoints (#3373) and RUN_ID-keyed main-clean baselines (#3768).
fn sweep_checkpoint_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".loom").join("sweep-checkpoint")
}

/// `<repo_root>/.loom/sweep-run/` — the sweep run registry written by
/// `sweep-run-registry.sh new`.
fn sweep_run_registry_dir(repo_root: &Path) -> PathBuf {
    repo_root.join(".loom").join("sweep-run")
}

/// Runtime dependencies of [`clean_sweep_transients`], injected so the
/// decision logic is unit-testable without a real clock, a live sweep, or
/// `gh` on PATH.
struct SweepTransientEnv<'a> {
    /// Wall clock the age guard measures against.
    now: SystemTime,
    /// Minimum age before a transient is eligible for pruning.
    min_age: Duration,
    /// `kill -0`-equivalent liveness probe for a registered run's PID.
    pid_alive: &'a dyn Fn(u32) -> bool,
    /// Forge issue-state probe: `"OPEN"` / `"CLOSED"` / `"UNKNOWN"`.
    issue_state: &'a dyn Fn(u32) -> String,
}

/// Whether `run_id` still names a live sweep run.
///
/// Fail-safe by construction: a *missing* registry entry is the only path to
/// "not live" other than a positively-dead PID. An entry that exists but whose
/// JSON (or `pid` field) cannot be read is treated as LIVE, so a corrupt
/// registry write never costs a running sweep its baseline.
fn sweep_run_is_live(repo_root: &Path, run_id: &str, pid_alive: &dyn Fn(u32) -> bool) -> bool {
    let entry = sweep_run_registry_dir(repo_root).join(format!("{run_id}.json"));
    let Ok(text) = std::fs::read_to_string(&entry) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return true;
    };
    let Some(pid) = value
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|p| u32::try_from(p).ok())
    else {
        return true;
    };
    pid_alive(pid)
}

/// Age of `path` relative to `now`. `None` when the mtime is unreadable
/// (caller treats that as "not eligible", never as "old enough to delete").
fn file_age(path: &Path, now: SystemTime) -> Option<Duration> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(now.duration_since(modified).unwrap_or_default())
}

/// Remove one transient file (or report it under `--dry-run`). Returns whether
/// the removal succeeded / would have happened.
fn remove_transient(path: &Path, label: &str, dry_run: bool) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if dry_run {
        println!("  Would remove {label}: {name}");
        return true;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {
            println!("  Removed {label}: {name}");
            true
        }
        Err(e) => {
            eprintln!("  Failed to remove {name}: {e}");
            false
        }
    }
}

/// Bulk prune of `.loom/sweep-checkpoint/` per-run transients (#4450).
///
/// `sweep-run-registry.sh cleanup` deletes a run's own baseline at sweep end,
/// but that hook is best-effort — a SIGKILLed sweep skips it, and per-issue
/// checkpoints for issues that are never re-swept live forever. This is the
/// backstop that keeps the directory bounded. Three categories:
///
/// 1. **RUN_ID-keyed baselines** (`main-clean-baseline-<RUN_ID>.txt`) whose run
///    is not live (no registry entry, or a registered PID that is dead) *and*
///    which are older than `min_age`.
/// 2. **The legacy un-keyed baseline** (`main-clean-baseline.txt`, pre-#3768,
///    in either its `.loom/sweep-checkpoint/` or older `.loom/` location) — no
///    live run can own it, so age does not matter.
/// 3. **Per-issue checkpoints** (`issue-<N>.json`) older than `min_age` whose
///    issue the forge reports CLOSED and which no in-flight sweep is tracking.
///
/// Every category fails safe: unknown issue state, unreadable mtime, an
/// unparseable registry entry, or an in-flight claim all mean *keep*. Files
/// that match neither naming pattern are never touched.
fn clean_sweep_transients_with(
    repo_root: &Path,
    stats: &mut CleanupStats,
    dry_run: bool,
    env: &SweepTransientEnv,
) {
    // Category 2 first: no liveness or age question to answer.
    for legacy in [
        sweep_checkpoint_dir(repo_root).join("main-clean-baseline.txt"),
        repo_root.join(".loom").join("main-clean-baseline.txt"),
    ] {
        if legacy.is_file() {
            if remove_transient(&legacy, "legacy un-keyed baseline", dry_run) {
                stats.cleaned_sweep_baselines += 1;
            } else {
                stats.errors += 1;
            }
        }
    }

    let dir = sweep_checkpoint_dir(repo_root);
    if !dir.is_dir() {
        println!("  No `.loom/sweep-checkpoint/` directory");
        return;
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        println!("  Could not read `.loom/sweep-checkpoint/`");
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::path);

    // Issues with an in-flight sweep right now (claim locks + spawn-loop
    // state). A daemon-owned sweep's checkpoint must survive even if its
    // issue already reads CLOSED on the forge.
    let live_issues = active_spawn_loop_issues(repo_root);

    for entry in entries {
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();

        // Already handled above (and only still visible under --dry-run).
        if name == "main-clean-baseline.txt" {
            continue;
        }

        if let Some(run_id) = name
            .strip_prefix("main-clean-baseline-")
            .and_then(|rest| rest.strip_suffix(".txt"))
        {
            if sweep_run_is_live(repo_root, run_id, env.pid_alive) {
                println!("  Keeping baseline of live sweep run: {name}");
                stats.kept_sweep_transients += 1;
                continue;
            }
            match file_age(&path, env.now) {
                Some(age) if age >= env.min_age => {
                    if remove_transient(&path, "stale sweep baseline", dry_run) {
                        stats.cleaned_sweep_baselines += 1;
                    } else {
                        stats.errors += 1;
                    }
                }
                _ => stats.kept_sweep_transients += 1,
            }
            continue;
        }

        if let Some(issue) = name
            .strip_prefix("issue-")
            .and_then(|rest| rest.strip_suffix(".json"))
            .and_then(|n| n.parse::<u32>().ok())
        {
            if live_issues.contains(&issue) {
                println!("  Keeping checkpoint of in-flight sweep: {name}");
                stats.kept_sweep_transients += 1;
                continue;
            }
            // The age gate also bounds how many forge probes one pass issues.
            match file_age(&path, env.now) {
                Some(age) if age >= env.min_age => {}
                _ => {
                    stats.kept_sweep_transients += 1;
                    continue;
                }
            }
            let state = (env.issue_state)(issue);
            if state == "CLOSED" {
                if remove_transient(&path, "closed-issue checkpoint", dry_run) {
                    stats.cleaned_sweep_checkpoints += 1;
                } else {
                    stats.errors += 1;
                }
            } else {
                println!("  Issue #{issue} is {state} - keeping {name}");
                stats.kept_sweep_transients += 1;
            }
        }
        // Anything else in the directory is not ours to delete.
    }
}

/// Production entry point for [`clean_sweep_transients_with`]: real clock,
/// [`SWEEP_TRANSIENT_MIN_AGE_SECS`], `kill -0` liveness, and the REST
/// issue-state probe (never GraphQL — see [`gh::issue_state_rest`]).
pub fn clean_sweep_transients(repo_root: &Path, stats: &mut CleanupStats, dry_run: bool) {
    let pid_alive = |pid: u32| crate::sweep_registry::is_pid_alive(pid);
    let issue_state = |issue: u32| gh::issue_state_rest(repo_root, issue);
    let env = SweepTransientEnv {
        now: SystemTime::now(),
        min_age: Duration::from_secs(SWEEP_TRANSIENT_MIN_AGE_SECS),
        pid_alive: &pid_alive,
        issue_state: &issue_state,
    };
    clean_sweep_transients_with(repo_root, stats, dry_run, &env);
}

fn dir_size_human(path: &Path) -> String {
    fn walk(path: &Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            if let Ok(ft) = entry.file_type() {
                if ft.is_dir() {
                    walk(&entry.path(), total);
                } else if let Ok(meta) = entry.metadata() {
                    *total += meta.len();
                }
            }
        }
    }
    let mut total = 0u64;
    walk(path, &mut total);
    if total >= 1024 * 1024 * 1024 {
        format!("{:.1}G", total as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if total >= 1024 * 1024 {
        format!("{:.1}M", total as f64 / (1024.0 * 1024.0))
    } else if total >= 1024 {
        format!("{:.1}K", total as f64 / 1024.0)
    } else {
        format!("{total}B")
    }
}

pub fn clean_build_artifacts(repo_root: &Path, dry_run: bool) {
    for name in ["target", "node_modules"] {
        let dir = repo_root.join(name);
        if dir.is_dir() {
            let size = dir_size_human(&dir);
            if dry_run {
                println!("Would remove {name}/ ({size})");
            } else if std::fs::remove_dir_all(&dir).is_ok() {
                println!("Removed {name}/ ({size})");
            } else {
                eprintln!("Failed to remove {name}/");
            }
        } else {
            println!("No {name}/ directory found");
        }
        println!();
    }
}

fn spawn_loop_locks_dir(repo_root: &Path) -> std::path::PathBuf {
    super::liveness::locks_dir(repo_root)
}

/// Remove `.loom/locks/issue-<N>/` dirs not backed by a live spawn-loop task.
/// Mirrors `clean.py::_clear_stale_spawn_loop_locks`.
pub fn clear_stale_spawn_loop_locks(repo_root: &Path, dry_run: bool) -> usize {
    let locks_dir = spawn_loop_locks_dir(repo_root);
    if !locks_dir.is_dir() {
        println!("  No `.loom/locks/` directory");
        return 0;
    }
    let state = super::spawn_loop_state::read_spawn_loop_state(repo_root);
    let live_issues: std::collections::HashSet<u32> = state
        .running
        .iter()
        .filter(|t| t.issue != 0)
        .map(|t| t.issue)
        .collect();

    let mut removed = 0usize;
    let mut found_any = false;
    let Ok(entries) = std::fs::read_dir(&locks_dir) else {
        return 0;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::path);
    for entry in entries {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("issue-") else {
            continue;
        };
        found_any = true;
        let Ok(issue_num) = rest.parse::<u32>() else {
            eprintln!("  Skipping malformed lock dir: {name}");
            continue;
        };
        if live_issues.contains(&issue_num) {
            println!("  Keeping lock for live task: {name}");
            continue;
        }
        if dry_run {
            println!("  Would remove stale lock: {name}");
            removed += 1;
        } else if std::fs::remove_dir_all(entry.path()).is_ok() {
            println!("  Removed stale lock: {name}");
            removed += 1;
        } else {
            eprintln!("  Failed to remove {name}");
        }
    }
    if !found_any {
        println!("  No spawn-loop locks to inspect");
    }
    removed
}

fn revert_stale_building_labels_spawn_loop(repo_root: &Path, dry_run: bool) -> usize {
    let state_present = super::spawn_loop_state::read_spawn_loop_state(repo_root).present;
    let locked_issues = super::liveness::active_locked_issues(repo_root);
    if !state_present && locked_issues.is_empty() {
        println!(
            "  No authoritative liveness source (no spawn-loop-state.json, no \
             .loom/locks/issue-<N>/ locks) — skipping loom:building revert \
             (fail-safe: absent liveness data means treat claims as ALIVE, \
             not orphaned). See issue #3651."
        );
        return 0;
    }

    let active = active_spawn_loop_issues(repo_root);
    let building = match gh::list_building_issues(repo_root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  Could not list building issues: {e}");
            return 0;
        }
    };

    let orphans: Vec<u32> = building
        .iter()
        .map(|b| b.number)
        .filter(|n| !active.contains(n))
        .collect();
    if orphans.is_empty() {
        println!("  No orphaned `loom:building` labels found");
        return 0;
    }

    let mut reverted = 0usize;
    for issue_num in orphans {
        if dry_run {
            println!("  Would revert label on #{issue_num}: building -> issue");
            continue;
        }
        match gh::edit_labels(repo_root, issue_num, "loom:building", "loom:issue") {
            Ok(()) => {
                println!("  Reverted #{issue_num}: building -> issue");
                reverted += 1;
            }
            Err(e) => eprintln!("  Failed to revert #{issue_num}: {e}"),
        }
    }
    reverted
}

/// `--daemon` crash recovery. Mirrors `clean.py::clean_daemon_crash_state`.
pub fn clean_daemon_crash_state(repo_root: &Path, dry_run: bool) {
    println!("Step 1: Kill orphaned tmux sessions");
    let mut stats = CleanupStats::default();
    clean_tmux_sessions(&mut stats, dry_run);
    println!();

    println!("Step 2: Revert stale `loom:building` labels");
    revert_stale_building_labels_spawn_loop(repo_root, dry_run);
    println!();

    println!("Step 3: Clear stale spawn-loop claim locks");
    clear_stale_spawn_loop_locks(repo_root, dry_run);
    println!();

    println!("Step 4: Reset issue-failures.json");
    let failures_file = repo_root.join(".loom").join("issue-failures.json");
    if failures_file.exists() {
        if dry_run {
            println!("Would reset: issue-failures.json");
        } else {
            let _ = std::fs::write(&failures_file, "{\n  \"entries\": {}\n}\n");
            println!("Reset issue-failures.json");
        }
    } else {
        println!("No issue-failures.json to reset");
    }
    println!();
}

pub fn print_summary(stats: &CleanupStats, dry_run: bool, safe_mode: bool) {
    println!();
    println!("========================================");
    println!("  Summary");
    println!("========================================");
    println!();
    if dry_run {
        println!("  Would clean: {} worktree(s)", stats.cleaned_worktrees);
    } else {
        println!("  Cleaned: {} worktree(s)", stats.cleaned_worktrees);
    }
    if stats.skipped_in_use > 0 {
        println!("  Skipped (in use by shepherd): {}", stats.skipped_in_use);
    }
    if stats.skipped_editable > 0 {
        println!("  Skipped (editable pip install): {}", stats.skipped_editable);
    }
    if safe_mode {
        println!("  Skipped (open/not merged): {}", stats.skipped_open + stats.skipped_not_merged);
        println!("  Skipped (grace period): {}", stats.skipped_grace);
        println!("  Skipped (uncommitted): {}", stats.skipped_uncommitted);
    }
    if stats.cleaned_branches > 0 || stats.kept_branches > 0 || stats.errored_branches > 0 {
        if dry_run {
            println!("  Would delete: {} branch(es)", stats.cleaned_branches);
        } else {
            println!("  Deleted: {} branch(es)", stats.cleaned_branches);
        }
        println!("  Kept: {} branch(es)", stats.kept_branches);
        if stats.errored_branches > 0 {
            println!("  Errored (gh probe failed): {} branch(es)", stats.errored_branches);
        }
    }
    if stats.killed_tmux > 0 {
        if dry_run {
            println!("  Would kill: {} tmux session(s)", stats.killed_tmux);
        } else {
            println!("  Killed: {} tmux session(s)", stats.killed_tmux);
        }
    }
    if stats.cleaned_config_dirs > 0 {
        if dry_run {
            println!("  Would remove: {} agent config dir(s)", stats.cleaned_config_dirs);
        } else {
            println!("  Removed: {} agent config dir(s)", stats.cleaned_config_dirs);
        }
    }
    if stats.cleaned_sweep_baselines > 0 || stats.cleaned_sweep_checkpoints > 0 {
        if dry_run {
            println!(
                "  Would remove: {} sweep baseline(s), {} closed-issue checkpoint(s)",
                stats.cleaned_sweep_baselines, stats.cleaned_sweep_checkpoints
            );
        } else {
            println!(
                "  Removed: {} sweep baseline(s), {} closed-issue checkpoint(s)",
                stats.cleaned_sweep_baselines, stats.cleaned_sweep_checkpoints
            );
        }
    }
    if stats.kept_sweep_transients > 0 {
        println!("  Kept: {} sweep transient(s)", stats.kept_sweep_transients);
    }
    if stats.errors > 0 {
        println!("  Errors: {}", stats.errors);
    }
    println!();
}

/// Run the standard (non-`--aggressive`, non-`--daemon`) clean pass. Returns
/// the process exit code (1 if any errors were recorded, else 0) — mirrors
/// `clean.py::main`'s non-interactive branches (this native port always runs
/// non-interactively: an unattended CLI has no stdin to prompt against, so
/// the "no flag given" case behaves like a safe no-op skip rather than
/// blocking on a prompt — see `clean_worktrees`'s final `else` branch).
pub fn run_clean(repo_root: &Path, opts: &CleanOptions) -> i32 {
    let all_targets = !opts.worktrees_only && !opts.branches_only && !opts.tmux_only;
    let mut stats = CleanupStats::default();

    println!();
    println!("========================================");
    if opts.deep {
        println!("  Loom Deep Cleanup");
    } else if opts.safe {
        println!("  Loom Safe Cleanup");
    } else {
        println!("  Loom Cleanup");
    }
    if opts.dry_run {
        println!("  (DRY RUN MODE)");
    }
    println!("========================================");
    println!();

    let confirmed = if opts.dry_run {
        println!("DRY RUN - No changes will be made");
        true
    } else if opts.force {
        println!("FORCE MODE - Auto-confirming all prompts");
        true
    } else {
        confirm("Proceed with cleanup? [y/N] ")
    };
    if !confirmed {
        println!("Cleanup cancelled");
        return 0;
    }
    println!();

    if !opts.branches_only && !opts.tmux_only {
        println!("Cleaning Worktrees\n");
        clean_worktrees(repo_root, &mut stats, opts);
        prune_orphaned_worktrees(repo_root, opts.dry_run);
        println!();
        println!("Cleaning Stale Spawn-Loop Locks\n");
        clear_stale_spawn_loop_locks(repo_root, opts.dry_run);
        println!();
    }

    if !opts.worktrees_only && !opts.tmux_only {
        println!("Cleaning Merged Branches\n");
        clean_branches(repo_root, &mut stats, opts.dry_run);
        println!();
    }

    if !opts.worktrees_only && !opts.branches_only {
        println!("Cleaning Loom Tmux Sessions\n");
        clean_tmux_sessions(&mut stats, opts.dry_run);
        println!();
    }

    if all_targets {
        println!("Cleaning Agent Config Directories\n");
        clean_agent_config(repo_root, &mut stats, opts.dry_run);
        println!();

        println!("Cleaning Sweep Checkpoint Transients\n");
        clean_sweep_transients(repo_root, &mut stats, opts.dry_run);
        println!();
    }

    if opts.deep {
        println!("Deep Cleaning Build Artifacts\n");
        clean_build_artifacts(repo_root, opts.dry_run);
        println!();
    }

    print_summary(&stats, opts.dry_run, opts.safe);

    if opts.dry_run {
        println!("Dry run complete - no changes made");
    } else {
        println!("Cleanup complete!");
    }

    i32::from(stats.errors > 0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn grace_period_not_passed_reports_remaining() {
        let now = Utc::now();
        let merged = now - chrono::Duration::seconds(100);
        let (passed, remaining) = check_grace_period(merged, 600, now);
        assert!(!passed);
        assert_eq!(remaining, 500);
    }

    #[test]
    fn grace_period_passed_reports_zero_remaining() {
        let now = Utc::now();
        let merged = now - chrono::Duration::seconds(700);
        let (passed, remaining) = check_grace_period(merged, 600, now);
        assert!(passed);
        assert_eq!(remaining, 0);
    }

    #[test]
    fn dir_size_human_handles_missing_dir() {
        // A missing directory contributes 0 bytes, not an error.
        assert_eq!(dir_size_human(Path::new("/does/not/exist/at/all")), "0B");
    }

    #[test]
    fn clear_stale_locks_no_dir_is_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(clear_stale_spawn_loop_locks(dir.path(), true), 0);
    }

    // --- sweep-checkpoint transient pruning (#4450) ---------------------

    const HOUR: u64 = 3600;

    /// Build a checkpoint-dir fixture and return `(tempdir, checkpoint_dir)`.
    fn sweep_fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let ckpt = sweep_checkpoint_dir(dir.path());
        std::fs::create_dir_all(&ckpt).unwrap();
        (dir, ckpt)
    }

    fn register_run(repo_root: &Path, run_id: &str, pid: u32) {
        let reg = sweep_run_registry_dir(repo_root);
        std::fs::create_dir_all(&reg).unwrap();
        std::fs::write(
            reg.join(format!("{run_id}.json")),
            format!(r#"{{"run_id": "{run_id}", "pid": {pid}, "timestamp": "now"}}"#),
        )
        .unwrap();
    }

    /// Run the pass with a clock advanced by `age_hours`, so every fixture
    /// file reads as exactly that old without touching filesystem mtimes.
    fn run_transients(
        repo_root: &Path,
        dry_run: bool,
        age_hours: u64,
        alive: &[u32],
        states: &[(u32, &str)],
    ) -> CleanupStats {
        let mut stats = CleanupStats::default();
        let alive: Vec<u32> = alive.to_vec();
        let states: Vec<(u32, String)> =
            states.iter().map(|(n, s)| (*n, (*s).to_string())).collect();
        let pid_alive = |pid: u32| alive.contains(&pid);
        let issue_state = |issue: u32| {
            states
                .iter()
                .find(|(n, _)| *n == issue)
                .map_or_else(|| "UNKNOWN".to_string(), |(_, s)| s.clone())
        };
        let env = SweepTransientEnv {
            now: SystemTime::now() + Duration::from_secs(age_hours * HOUR),
            min_age: Duration::from_secs(SWEEP_TRANSIENT_MIN_AGE_SECS),
            pid_alive: &pid_alive,
            issue_state: &issue_state,
        };
        clean_sweep_transients_with(repo_root, &mut stats, dry_run, &env);
        stats
    }

    #[test]
    fn sweep_transients_missing_dir_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let stats = run_transients(dir.path(), false, 100, &[], &[]);
        assert_eq!(stats.cleaned_sweep_baselines, 0);
        assert_eq!(stats.cleaned_sweep_checkpoints, 0);
        assert_eq!(stats.errors, 0);
    }

    #[test]
    fn sweep_transients_prunes_orphan_baseline_past_threshold() {
        let (dir, ckpt) = sweep_fixture();
        let orphan = ckpt.join("main-clean-baseline-sweep-dead.txt");
        std::fs::write(&orphan, "").unwrap();
        let stats = run_transients(dir.path(), false, 100, &[], &[]);
        assert!(!orphan.exists());
        assert_eq!(stats.cleaned_sweep_baselines, 1);
    }

    #[test]
    fn sweep_transients_keeps_young_orphan_baseline() {
        let (dir, ckpt) = sweep_fixture();
        let young = ckpt.join("main-clean-baseline-sweep-dead.txt");
        std::fs::write(&young, "").unwrap();
        let stats = run_transients(dir.path(), false, 1, &[], &[]);
        assert!(young.exists(), "mtime guard must spare a young baseline");
        assert_eq!(stats.cleaned_sweep_baselines, 0);
        assert_eq!(stats.kept_sweep_transients, 1);
    }

    #[test]
    fn sweep_transients_keep_live_run_baseline_regardless_of_age() {
        let (dir, ckpt) = sweep_fixture();
        let live = ckpt.join("main-clean-baseline-sweep-live.txt");
        std::fs::write(&live, "").unwrap();
        register_run(dir.path(), "sweep-live", 4242);
        // 1000h old, but the registered PID is alive.
        let stats = run_transients(dir.path(), false, 1000, &[4242], &[]);
        assert!(live.exists(), "a live run's baseline must never be pruned");
        assert_eq!(stats.cleaned_sweep_baselines, 0);
        assert_eq!(stats.kept_sweep_transients, 1);
    }

    /// #4691: a run whose PID exists but cannot be signalled by this process
    /// (`kill(2)` → `EPERM`) is LIVE. Wiring the real
    /// [`crate::sweep_registry::is_pid_alive_with`] decision core in here — with
    /// only the raw syscall mocked — proves the production `pid_alive` closure,
    /// not just the test double, keeps such a baseline.
    #[cfg(unix)]
    #[test]
    fn sweep_transients_keep_baseline_of_unsignallable_but_live_run() {
        use crate::sweep_registry::{is_pid_alive_with, EPERM};

        let (dir, ckpt) = sweep_fixture();
        let baseline = ckpt.join("main-clean-baseline-sweep-eperm.txt");
        std::fs::write(&baseline, "").unwrap();
        register_run(dir.path(), "sweep-eperm", 4242);

        let pid_alive = |pid: u32| is_pid_alive_with(pid, |_| Err(EPERM));
        let issue_state = |_: u32| "UNKNOWN".to_string();
        let mut stats = CleanupStats::default();
        let env = SweepTransientEnv {
            // 1000h old: only the liveness verdict can spare it.
            now: SystemTime::now() + Duration::from_secs(1000 * HOUR),
            min_age: Duration::from_secs(SWEEP_TRANSIENT_MIN_AGE_SECS),
            pid_alive: &pid_alive,
            issue_state: &issue_state,
        };
        clean_sweep_transients_with(dir.path(), &mut stats, false, &env);

        assert!(
            baseline.exists(),
            "an unsignallable (EPERM) PID means the sweep is still running — \
             its baseline must not be pruned"
        );
        assert_eq!(stats.cleaned_sweep_baselines, 0);
        assert_eq!(stats.kept_sweep_transients, 1);
    }

    /// The ESRCH counterpart of the test above: the same wiring, but the raw
    /// syscall reports "no such process" — the one failure mode that really does
    /// authorize pruning. Guards against an over-broad #4691 fix that makes
    /// every `kill(2)` failure mean "alive" and silently reinstates the leak.
    #[cfg(unix)]
    #[test]
    fn sweep_transients_still_prune_baseline_when_pid_is_esrch() {
        use crate::sweep_registry::is_pid_alive_with;
        const ESRCH: i32 = 3;

        let (dir, ckpt) = sweep_fixture();
        let baseline = ckpt.join("main-clean-baseline-sweep-gone.txt");
        std::fs::write(&baseline, "").unwrap();
        register_run(dir.path(), "sweep-gone", 4242);

        let pid_alive = |pid: u32| is_pid_alive_with(pid, |_| Err(ESRCH));
        let issue_state = |_: u32| "UNKNOWN".to_string();
        let mut stats = CleanupStats::default();
        let env = SweepTransientEnv {
            now: SystemTime::now() + Duration::from_secs(1000 * HOUR),
            min_age: Duration::from_secs(SWEEP_TRANSIENT_MIN_AGE_SECS),
            pid_alive: &pid_alive,
            issue_state: &issue_state,
        };
        clean_sweep_transients_with(dir.path(), &mut stats, false, &env);

        assert!(!baseline.exists(), "ESRCH means gone — prune must still fire");
        assert_eq!(stats.cleaned_sweep_baselines, 1);
    }

    #[test]
    fn sweep_transients_prunes_registered_but_dead_pid_baseline() {
        let (dir, ckpt) = sweep_fixture();
        let dead = ckpt.join("main-clean-baseline-sweep-crashed.txt");
        std::fs::write(&dead, "").unwrap();
        // Registry entry survives a SIGKILL — the PID liveness check is what
        // distinguishes it from a running sweep.
        register_run(dir.path(), "sweep-crashed", 999_999);
        let stats = run_transients(dir.path(), false, 100, &[], &[]);
        assert!(!dead.exists());
        assert_eq!(stats.cleaned_sweep_baselines, 1);
    }

    #[test]
    fn sweep_transients_keeps_baseline_with_unparseable_registry_entry() {
        let (dir, ckpt) = sweep_fixture();
        let path = ckpt.join("main-clean-baseline-sweep-corrupt.txt");
        std::fs::write(&path, "").unwrap();
        let reg = sweep_run_registry_dir(dir.path());
        std::fs::create_dir_all(&reg).unwrap();
        std::fs::write(reg.join("sweep-corrupt.json"), "{not json").unwrap();
        let stats = run_transients(dir.path(), false, 100, &[], &[]);
        assert!(path.exists(), "corrupt registry entry must fail safe (keep)");
        assert_eq!(stats.cleaned_sweep_baselines, 0);
    }

    #[test]
    fn sweep_transients_removes_legacy_unkeyed_baselines() {
        let (dir, ckpt) = sweep_fixture();
        let legacy = ckpt.join("main-clean-baseline.txt");
        std::fs::write(&legacy, "").unwrap();
        let older = dir.path().join(".loom").join("main-clean-baseline.txt");
        std::fs::write(&older, "").unwrap();
        // Age 0: the legacy files have no owner, so the threshold does not apply.
        let stats = run_transients(dir.path(), false, 0, &[], &[]);
        assert!(!legacy.exists());
        assert!(!older.exists());
        assert_eq!(stats.cleaned_sweep_baselines, 2);
    }

    #[test]
    fn sweep_transients_ignores_unrelated_files() {
        let (dir, ckpt) = sweep_fixture();
        let other = ckpt.join("notes.txt");
        std::fs::write(&other, "").unwrap();
        let weird = ckpt.join("main-clean-baseline-sweep-x.json");
        std::fs::write(&weird, "").unwrap();
        let stats = run_transients(dir.path(), false, 1000, &[], &[]);
        assert!(other.exists());
        assert!(weird.exists());
        assert_eq!(stats.cleaned_sweep_baselines, 0);
        assert_eq!(stats.cleaned_sweep_checkpoints, 0);
    }

    #[test]
    fn sweep_transients_dry_run_deletes_nothing_but_counts() {
        let (dir, ckpt) = sweep_fixture();
        let baseline = ckpt.join("main-clean-baseline-sweep-dead.txt");
        std::fs::write(&baseline, "").unwrap();
        let legacy = ckpt.join("main-clean-baseline.txt");
        std::fs::write(&legacy, "").unwrap();
        let checkpoint = ckpt.join("issue-3784.json");
        std::fs::write(&checkpoint, "{}").unwrap();
        let stats = run_transients(dir.path(), true, 100, &[], &[(3784, "CLOSED")]);
        assert!(baseline.exists());
        assert!(legacy.exists());
        assert!(checkpoint.exists());
        assert_eq!(stats.cleaned_sweep_baselines, 2);
        assert_eq!(stats.cleaned_sweep_checkpoints, 1);
    }

    #[test]
    fn sweep_transients_prunes_closed_issue_checkpoint_only() {
        let (dir, ckpt) = sweep_fixture();
        let closed = ckpt.join("issue-3784.json");
        let open = ckpt.join("issue-4450.json");
        let unknown = ckpt.join("issue-4451.json");
        for p in [&closed, &open, &unknown] {
            std::fs::write(p, "{}").unwrap();
        }
        let stats =
            run_transients(dir.path(), false, 100, &[], &[(3784, "CLOSED"), (4450, "OPEN")]);
        assert!(!closed.exists());
        assert!(open.exists(), "OPEN issue checkpoint must be kept");
        assert!(unknown.exists(), "an unverified issue state must never delete");
        assert_eq!(stats.cleaned_sweep_checkpoints, 1);
        assert_eq!(stats.kept_sweep_transients, 2);
    }

    #[test]
    fn sweep_transients_keeps_young_closed_issue_checkpoint() {
        let (dir, ckpt) = sweep_fixture();
        let closed = ckpt.join("issue-3784.json");
        std::fs::write(&closed, "{}").unwrap();
        let stats = run_transients(dir.path(), false, 1, &[], &[(3784, "CLOSED")]);
        assert!(closed.exists(), "age gate also bounds forge probes");
        assert_eq!(stats.cleaned_sweep_checkpoints, 0);
    }

    #[test]
    fn sweep_transients_keeps_checkpoint_of_in_flight_sweep() {
        let (dir, ckpt) = sweep_fixture();
        let inflight = ckpt.join("issue-3784.json");
        std::fs::write(&inflight, "{}").unwrap();
        // A daemon-owned sweep holds a claim lock for this issue.
        std::fs::create_dir_all(super::super::liveness::locks_dir(dir.path()).join("issue-3784"))
            .unwrap();
        let stats = run_transients(dir.path(), false, 1000, &[], &[(3784, "CLOSED")]);
        assert!(
            inflight.exists(),
            "an in-flight sweep's checkpoint must survive even when its issue is CLOSED"
        );
        assert_eq!(stats.cleaned_sweep_checkpoints, 0);
        assert_eq!(stats.kept_sweep_transients, 1);
    }

    #[test]
    fn clear_stale_locks_keeps_live_and_removes_dead() {
        let dir = tempfile::tempdir().unwrap();
        let locks = spawn_loop_locks_dir(dir.path());
        std::fs::create_dir_all(locks.join("issue-1")).unwrap();
        std::fs::create_dir_all(locks.join("issue-2")).unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom").join("spawn-loop-state.json"),
            r#"{"running": [{"issue": 1, "pid": 1}]}"#,
        )
        .unwrap();
        let removed = clear_stale_spawn_loop_locks(dir.path(), false);
        assert_eq!(removed, 1);
        assert!(locks.join("issue-1").exists());
        assert!(!locks.join("issue-2").exists());
    }
}
