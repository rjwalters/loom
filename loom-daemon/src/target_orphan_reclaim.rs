//! Orphan sweep for cargo target dirs that no worktree removal will ever
//! reach (issue #8370, part b).
//!
//! # What this collects
//!
//! Two kinds of directory, under a fixed list of prefixes and nowhere else:
//!
//! * Loom-owned run dirs, `<repo>/.loom/targets/<role>-<run-id>`
//!   ([`crate::run_target_dir`]). A role-runner tick removes its own at run
//!   end; a daemon sweep or a manual spawn has no process left to do that
//!   (`worker_spawn` ends in `exec()`), so this is where those go.
//! * The locations agents improvised before Loom gave them one:
//!   `<repo>/.loom/target-*`, `/tmp/loom-target-*`, `/tmp/cargo-target-*`,
//!   `$TMPDIR/cargo-target-*`, `~/.cache/cargo-target-*`. One fleet host held
//!   85 GB of `.loom/target-{builder,doctor,judge}-<N>`; another 45 GB of
//!   `/private/tmp/cargo-target-{issue,review}-*`.
//!
//! Out of scope: Claude Code session scratchpads and `$TMPDIR/opencode/*`
//! clones. Neither has a stable prefix this module could match without also
//! matching things that are not build output.
//!
//! # Safety: every gate must pass, and an unknown is a keep
//!
//! A candidate is removed only when ALL of these hold:
//!
//! 1. It is a direct child of one of the scan roots and its name matches that
//!    root's prefix. Nothing is found by walking deeper.
//! 2. It is a real directory, not a symlink (`symlink_metadata`), so a link
//!    named `cargo-target-x` pointing at `$HOME` is never followed.
//! 3. It does not overlap a configured target dir
//!    ([`host_configured_target_dirs`]): an
//!    operator's shared `CARGO_TARGET_DIR=/tmp/cargo-target-shared` is not an
//!    orphan.
//! 4. The newest mtime anywhere under it (not the directory's own mtime) is
//!    older than the max age (default [`DEFAULT_MAX_AGE_HOURS`]).
//! 5. No live claim names its issue number, and a run dir's recorded owner pid
//!    ([`crate::run_target_dir::OWNER_FILE`]) is not running.
//! 6. No process holds anything open under it
//!    ([`crate::worktree_ops::safety::find_processes_using_directory`]). When
//!    neither `/proc` nor `lsof` is available that probe cannot answer, and
//!    the candidate is **kept**: the shared probe degrades to "no holders",
//!    which is fine for a defence-in-depth gate and wrong for this one.
//!
//! # Where it runs
//!
//! The below-floor tier ([`crate::eager_reclaim`]), the scheduled 15-minute
//! tier ([`crate::worktree_reaper::reap_repo`]) and `loom-daemon clean`
//! (report-only without `--force`, and always under `--dry-run`). The two
//! daemon tiers share one per-repo cooldown; `clean` bypasses it.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

/// The category every reclaim log line carries.
pub const CATEGORY: &str = "cargo_target_orphan";

/// Master on/off env override (default on).
pub const ENABLE_ENV: &str = "LOOM_TARGET_ORPHAN_RECLAIM";
/// Env override for the max age (hours).
pub const MAX_AGE_HOURS_ENV: &str = "LOOM_TARGET_ORPHAN_RECLAIM_MAX_AGE_HOURS";
/// Env override for the per-repo cooldown (seconds).
pub const MIN_INTERVAL_ENV: &str = "LOOM_TARGET_ORPHAN_RECLAIM_MIN_INTERVAL_SECS";

/// Default max age: 3 hours since the last write anywhere under the dir. Every
/// leaked dir measured on 2026-10-06 and 2026-10-08 was older than this, and
/// a live build writes far more often.
pub const DEFAULT_MAX_AGE_HOURS: u64 = 3;
/// Default cooldown between passes for one repo: 30 minutes.
pub const DEFAULT_MIN_INTERVAL_SECS: u64 = 1_800;

/// Name prefixes for the improvised locations.
pub const LEGACY_REPO_PREFIX: &str = "target-";
/// Prefixes matched directly under `/tmp`.
pub const TMP_PREFIXES: &[&str] = &["loom-target-", "cargo-target-"];
/// Prefix matched directly under `$TMPDIR` and `~/.cache`.
pub const CARGO_TARGET_PREFIX: &str = "cargo-target-";

// ============================================================================
// Config (.loom/config.json → autonomous.worktreeReaper.targetOrphanReclaim)
// ============================================================================

/// Precedence for every field is env > config > default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetOrphanConfig {
    pub enabled: Option<bool>,
    pub max_age_hours: Option<u64>,
    pub min_interval_secs: Option<u64>,
}

#[must_use]
pub fn read_config(repo_root: &Path) -> TargetOrphanConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(
        &effective,
        "autonomous.worktreeReaper.targetOrphanReclaim",
    ) else {
        return TargetOrphanConfig::default();
    };
    TargetOrphanConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        max_age_hours: block
            .get("maxAgeHours")
            .and_then(serde_json::Value::as_u64)
            .filter(|&h| h > 0),
        min_interval_secs: block
            .get("minIntervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

#[must_use]
pub fn resolve_enabled(config: &TargetOrphanConfig) -> bool {
    if let Ok(v) = std::env::var(ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// A zero or unparseable value falls through: a 0-hour floor would make every
/// matching dir eligible the moment its build paused.
#[must_use]
pub fn resolve_max_age_hours(config: &TargetOrphanConfig) -> u64 {
    std::env::var(MAX_AGE_HOURS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&h| h > 0)
        .or(config.max_age_hours)
        .unwrap_or(DEFAULT_MAX_AGE_HOURS)
}

#[must_use]
pub fn resolve_min_interval_secs(config: &TargetOrphanConfig) -> u64 {
    std::env::var(MIN_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.min_interval_secs)
        .unwrap_or(DEFAULT_MIN_INTERVAL_SECS)
}

// ============================================================================
// Scan roots and the name predicate
// ============================================================================

/// One directory whose direct children are candidates, and which names count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRoot {
    pub dir: PathBuf,
    pub matcher: NameMatcher,
}

/// Which child names under a [`ScanRoot`] are candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameMatcher {
    /// `<repo>/.loom/targets/`: any run-dir-shaped name.
    RunDir,
    /// Any of these prefixes, followed by at least one more character.
    Prefixes(Vec<&'static str>),
}

impl NameMatcher {
    #[must_use]
    pub fn matches(&self, name: &str) -> bool {
        match self {
            Self::RunDir => {
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            }
            Self::Prefixes(prefixes) => prefixes
                .iter()
                .any(|p| name.len() > p.len() && name.starts_with(p)),
        }
    }
}

/// The fixed scan roots for `repo_root`, with `home` and `tmpdir` injected.
/// A root that resolves to the same real directory as an earlier one (macOS
/// `$TMPDIR` is never `/tmp`, but a Linux one often is) is merged into it.
#[must_use]
pub fn scan_roots(repo_root: &Path, home: Option<&Path>, tmpdir: Option<&Path>) -> Vec<ScanRoot> {
    let mut roots = vec![
        ScanRoot {
            dir: crate::run_target_dir::targets_root(repo_root),
            matcher: NameMatcher::RunDir,
        },
        ScanRoot {
            dir: repo_root.join(".loom"),
            matcher: NameMatcher::Prefixes(vec![LEGACY_REPO_PREFIX]),
        },
        ScanRoot {
            dir: PathBuf::from("/tmp"),
            matcher: NameMatcher::Prefixes(TMP_PREFIXES.to_vec()),
        },
    ];
    if let Some(tmpdir) = tmpdir.filter(|p| p.is_absolute()) {
        roots.push(ScanRoot {
            dir: tmpdir.to_path_buf(),
            matcher: NameMatcher::Prefixes(vec![CARGO_TARGET_PREFIX]),
        });
    }
    if let Some(home) = home.filter(|p| p.is_absolute()) {
        roots.push(ScanRoot {
            dir: home.join(".cache"),
            matcher: NameMatcher::Prefixes(vec![CARGO_TARGET_PREFIX]),
        });
    }
    let mut merged: Vec<(PathBuf, ScanRoot)> = Vec::new();
    for root in roots {
        let real = crate::worktree_ops::cargo_target::realish(&root.dir);
        if let Some((_, existing)) = merged.iter_mut().find(|(r, _)| *r == real) {
            if let (NameMatcher::Prefixes(have), NameMatcher::Prefixes(add)) =
                (&mut existing.matcher, &root.matcher)
            {
                for p in add {
                    if !have.contains(p) {
                        have.push(p);
                    }
                }
            }
            continue;
        }
        merged.push((real, root));
    }
    merged.into_iter().map(|(_, r)| r).collect()
}

// ============================================================================
// Per-candidate evaluation
// ============================================================================

/// Why a matching directory was kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepReason {
    /// A symlink or a non-directory wearing a matching name.
    NotADirectory,
    /// Overlaps a configured target dir.
    ConfiguredTargetDir(PathBuf),
    /// Written within the max age.
    Young { age_secs: i64 },
    /// A live claim lock names this issue.
    LiveClaim(u32),
    /// The run dir's recorded owner is still running.
    OwnerAlive(u32),
    /// A process holds something open under it.
    OpenHandle,
    /// The open-handle probe could not run (no `/proc`, no `lsof`).
    ProbeUnavailable,
    /// Its metadata could not be read.
    Unreadable,
}

/// A directory that passed every gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub newest_mtime: DateTime<Utc>,
}

/// The liveness inputs, injected so every gate is testable.
pub struct Probes<'a> {
    /// `Some(true)` held open, `Some(false)` free, `None` could not probe.
    pub open_handles: &'a dyn Fn(&Path) -> Option<bool>,
    pub owner_alive: &'a dyn Fn(u32) -> bool,
    /// Issue numbers with a live claim.
    pub live_issues: &'a HashSet<u32>,
    /// Configured target dirs (already resolved through symlinks).
    pub protected: &'a [PathBuf],
}

/// Every run of ASCII digits in `name`, parsed. `cargo-target-review-9745`
/// yields `[9745]`.
fn issue_numbers(name: &str) -> Vec<u32> {
    name.split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// The newest mtime anywhere under `path`, and the total size, without
/// following symlinks. An unreadable child is skipped.
fn newest_mtime_and_size(path: &Path) -> std::io::Result<(DateTime<Utc>, u64)> {
    let meta = std::fs::symlink_metadata(path)?;
    let own: DateTime<Utc> = meta.modified().map_or_else(|_| Utc::now(), DateTime::from);
    if !meta.is_dir() {
        return Ok((own, meta.len()));
    }
    let mut newest = own;
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok((child, size)) = newest_mtime_and_size(&entry.path()) {
                newest = newest.max(child);
                total = total.saturating_add(size);
            }
        }
    }
    Ok((newest, total))
}

/// Run every gate on one name-matched path, cheapest first. The open-handle
/// probe (an `lsof +D` walk on macOS) runs last, only for a dir that is
/// otherwise eligible.
pub fn evaluate(
    path: &Path,
    now: DateTime<Utc>,
    max_age_secs: i64,
    probes: &Probes<'_>,
) -> Result<Candidate, KeepReason> {
    let meta = std::fs::symlink_metadata(path).map_err(|_| KeepReason::Unreadable)?;
    if !meta.is_dir() {
        return Err(KeepReason::NotADirectory);
    }
    let real = crate::worktree_ops::cargo_target::realish(path);
    if let Some(p) = probes
        .protected
        .iter()
        .find(|p| real == **p || p.starts_with(&real) || real.starts_with(p))
    {
        return Err(KeepReason::ConfiguredTargetDir(p.clone()));
    }
    let (newest_mtime, size_bytes) =
        newest_mtime_and_size(path).map_err(|_| KeepReason::Unreadable)?;
    let age_secs = (now - newest_mtime).num_seconds();
    if age_secs < max_age_secs.max(1) {
        return Err(KeepReason::Young { age_secs });
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if let Some(n) = issue_numbers(name)
        .into_iter()
        .find(|n| probes.live_issues.contains(n))
    {
        return Err(KeepReason::LiveClaim(n));
    }
    if let Some(pid) = crate::run_target_dir::owner_pid(path).filter(|&p| (probes.owner_alive)(p)) {
        return Err(KeepReason::OwnerAlive(pid));
    }
    match (probes.open_handles)(path) {
        Some(false) => Ok(Candidate {
            path: path.to_path_buf(),
            size_bytes,
            newest_mtime,
        }),
        Some(true) => Err(KeepReason::OpenHandle),
        None => Err(KeepReason::ProbeUnavailable),
    }
}

// ============================================================================
// The pass
// ============================================================================

/// One pass's outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetOrphanReport {
    pub repo_root: PathBuf,
    pub enabled: bool,
    pub dry_run: bool,
    /// Present when the cooldown skipped this evaluation.
    pub deferred: Option<String>,
    /// Passed every gate (on a dry run, what WOULD be removed).
    pub eligible: Vec<Candidate>,
    /// Actually removed.
    pub removed: Vec<Candidate>,
    /// Name-matched but kept, with why.
    pub kept: Vec<(PathBuf, KeepReason)>,
    /// Eligible but `remove_dir_all` failed.
    pub failed: Vec<(PathBuf, String)>,
}

impl TargetOrphanReport {
    #[must_use]
    pub fn bytes_freed(&self) -> u64 {
        self.removed.iter().map(|c| c.size_bytes).sum()
    }

    #[must_use]
    pub fn bytes_eligible(&self) -> u64 {
        self.eligible.iter().map(|c| c.size_bytes).sum()
    }

    /// `"3 dir(s) (14.2G)"` or `"nothing"`; the eager tier's summary fragment.
    #[must_use]
    pub fn summary(&self) -> String {
        let (n, bytes) = if self.dry_run {
            (self.eligible.len(), self.bytes_eligible())
        } else {
            (self.removed.len(), self.bytes_freed())
        };
        if n == 0 {
            return "nothing".to_string();
        }
        format!("{n} dir(s) ({})", crate::tmpfs_reclaim::human_size(bytes))
    }

    /// The structured log line: `category=cargo_target_orphan` plus counts and
    /// bytes, so a log query can sum what this pass freed across the fleet.
    #[must_use]
    pub fn log_line(&self) -> String {
        format!(
            "target_orphan_reclaim: category={CATEGORY} repo={} dry_run={} removed={} \
             bytes_freed={} eligible={} bytes_eligible={} kept={} failed={} ({})",
            self.repo_root.display(),
            self.dry_run,
            self.removed.len(),
            self.bytes_freed(),
            self.eligible.len(),
            self.bytes_eligible(),
            self.kept.len(),
            self.failed.len(),
            self.summary(),
        )
    }
}

/// Log one pass: `info` when it removed (or would remove) anything or a
/// removal failed, `debug` otherwise.
pub fn log_report(report: &TargetOrphanReport) {
    if !report.enabled {
        log::debug!(
            "target_orphan_reclaim: {} disabled ({ENABLE_ENV} or \
             autonomous.worktreeReaper.targetOrphanReclaim.enabled)",
            report.repo_root.display()
        );
        return;
    }
    if let Some(reason) = &report.deferred {
        log::debug!("target_orphan_reclaim: {} skipped: {reason}", report.repo_root.display());
        return;
    }
    for (path, why) in &report.failed {
        log::warn!(
            "target_orphan_reclaim: category={CATEGORY} could not remove {}: {why}",
            path.display()
        );
    }
    if report.removed.is_empty() && report.eligible.is_empty() && report.failed.is_empty() {
        log::debug!("{}", report.log_line());
    } else {
        log::info!("{}", report.log_line());
    }
}

/// Scan `roots`, evaluate every name-matched child, and (unless `dry_run`)
/// remove the eligible ones. The injectable core; nothing here reads the
/// environment.
#[must_use]
pub fn run_with(
    repo_root: &Path,
    roots: &[ScanRoot],
    now: DateTime<Utc>,
    max_age_secs: i64,
    dry_run: bool,
    probes: &Probes<'_>,
) -> TargetOrphanReport {
    let mut report = TargetOrphanReport {
        repo_root: repo_root.to_path_buf(),
        enabled: true,
        dry_run,
        ..TargetOrphanReport::default()
    };
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root.dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !root.matcher.matches(name) {
                continue;
            }
            let path = entry.path();
            match evaluate(&path, now, max_age_secs, probes) {
                Ok(candidate) => report.eligible.push(candidate),
                Err(why) => report.kept.push((path, why)),
            }
        }
    }
    if !dry_run {
        for candidate in &report.eligible {
            match std::fs::remove_dir_all(&candidate.path) {
                Ok(()) => report.removed.push(candidate.clone()),
                Err(e) => report.failed.push((candidate.path.clone(), e.to_string())),
            }
        }
    }
    report
}

/// Every host- or repo-configured cargo target dir that applies to builds in
/// `repo_root`: this process's `CARGO_TARGET_DIR`, and `build.target-dir` in
/// `<repo_root>/.cargo/config.toml`, any ancestor's, or `$CARGO_HOME`'s,
/// resolved through symlinks. Starting the ancestor walk at
/// `<repo_root>/.loom` makes it include `repo_root` itself.
#[must_use]
pub fn host_configured_target_dirs(repo_root: &Path) -> Vec<PathBuf> {
    use crate::worktree_ops::cargo_target as ct;
    ct::machine_global_target_dirs_with(
        &repo_root.join(".loom"),
        ct::env_cargo_target_dir().as_deref(),
        ct::cargo_home().as_deref(),
    )
    .into_iter()
    .map(|(path, _)| ct::realish(&path))
    .collect()
}

/// The production open-handle probe. `None` when neither `/proc` nor `lsof`
/// is available: `find_processes_using_directory` would then report "no
/// holders", and this pass must not read that as "free".
fn production_open_handles(path: &Path) -> Option<bool> {
    let probe_available = (cfg!(target_os = "linux") && Path::new("/proc/self").is_dir())
        || std::process::Command::new("lsof")
            .arg("-v")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok();
    if !probe_available {
        return None;
    }
    Some(!crate::worktree_ops::safety::find_processes_using_directory(path).is_empty())
}

/// One pass with the production probes, no cooldown. `loom-daemon clean` calls
/// this directly.
#[must_use]
pub fn run_now(repo_root: &Path, max_age_hours: u64, dry_run: bool) -> TargetOrphanReport {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let tmpdir = std::env::var_os("TMPDIR").map(PathBuf::from);
    let mut roots = scan_roots(repo_root, home.as_deref(), tmpdir.as_deref());
    // A unit test that reaches this through a reaper or eager pass must never
    // act on the developer's real `/tmp`, `$TMPDIR` or `~/.cache`.
    if cfg!(test) {
        roots.retain(|r| r.dir.starts_with(repo_root));
    }
    let protected = host_configured_target_dirs(repo_root);
    let live_issues = crate::worktree_ops::liveness::active_spawn_loop_issues(repo_root);
    let probes = Probes {
        open_handles: &production_open_handles,
        owner_alive: &crate::live_claim::pid_is_live_process,
        live_issues: &live_issues,
        protected: &protected,
    };
    let max_age_secs = i64::try_from(max_age_hours.saturating_mul(3600)).unwrap_or(i64::MAX);
    run_with(repo_root, &roots, Utc::now(), max_age_secs, dry_run, &probes)
}

// ============================================================================
// Per-repo cooldown (shared by the eager and scheduled tiers)
// ============================================================================

static LAST_RUN_AT: OnceLock<Mutex<BTreeMap<PathBuf, DateTime<Utc>>>> = OnceLock::new();

fn last_run_slot() -> &'static Mutex<BTreeMap<PathBuf, DateTime<Utc>>> {
    LAST_RUN_AT.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Drop cooldown state. Test-only seam.
#[doc(hidden)]
pub fn reset_state_for_test() {
    last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// `Some(reason)` when a pass for `repo_root` ran inside the cooldown.
fn cooldown_reason(repo_root: &Path, now: DateTime<Utc>, min_interval_secs: u64) -> Option<String> {
    let last = last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(repo_root)
        .copied()?;
    let since = (now - last).num_seconds();
    (since >= 0 && since < i64::try_from(min_interval_secs).unwrap_or(i64::MAX))
        .then(|| format!("a pass ran {since}s ago (cooldown {min_interval_secs}s)"))
}

fn record_run(repo_root: &Path, now: DateTime<Utc>) {
    last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(repo_root.to_path_buf(), now);
}

/// One production pass for the daemon tiers: config, enable switch, per-repo
/// cooldown, then [`run_now`]. Logs its own report.
#[must_use]
pub fn run_for(repo_root: &Path) -> TargetOrphanReport {
    let config = read_config(repo_root);
    let now = Utc::now();
    let base = TargetOrphanReport {
        repo_root: repo_root.to_path_buf(),
        ..TargetOrphanReport::default()
    };
    if !resolve_enabled(&config) {
        log_report(&base);
        return base;
    }
    if let Some(reason) = cooldown_reason(repo_root, now, resolve_min_interval_secs(&config)) {
        let report = TargetOrphanReport {
            enabled: true,
            deferred: Some(reason),
            ..base
        };
        log_report(&report);
        return report;
    }
    record_run(repo_root, now);
    let report = run_now(repo_root, resolve_max_age_hours(&config), false);
    log_report(&report);
    report
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
