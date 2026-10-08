//! Reclaim of aborted-fetch debris inside a managed checkout's `.git/objects`
//! (#10995): `objects/pack/tmp_pack_*` and `objects/??/tmp_obj_*`.
//!
//! # Why this exists
//!
//! When a host's disk fills, every `git fetch` that runs out of space aborts
//! and leaves its partial download behind as `objects/pack/tmp_pack_*` (and,
//! for loose objects, `objects/??/tmp_obj_*`). Git only removes them during
//! `gc`/`prune`, and only once they are older than `gc.pruneExpire` (two weeks
//! by default). The daemon retries its fetches every tick, so a full disk fed
//! itself: on loom-worker-1 (2026-10-08) one checkout carried 35 GiB of
//! `size-garbage` against a 10 GiB real pack, and deleting it took the host
//! from 63 MB to 103 GB free.
//!
//! # Safety: three gates, all fail-closed
//!
//! This module deletes files inside `.git/objects`, where the only thing that
//! separates debris from a fetch in flight is a heuristic. So:
//!
//! 1. **Name and place** ([`is_tmp_debris_path`]): only a regular file named
//!    `tmp_pack_*` directly inside `objects/pack`, or `tmp_obj_*` directly
//!    inside a two-hex-digit loose-object fan-out directory `objects/??`.
//!    Real packs (`pack-*.pack`/`.idx`/`.rev`/`.keep`), `tmp_idx_*`, refs,
//!    `info/`, symlinks and anything at another depth are never candidates.
//! 2. **Age** ([`is_reclaimable_age`]): mtime at least the grace period old
//!    (default 60 minutes). A running fetch writes its temp file
//!    continuously, so its mtime stays fresh for as long as it is alive.
//! 3. **Liveness** ([`probe_git_liveness`]): the whole checkout is skipped
//!    while any `git` process has its cwd, or an open file, inside the
//!    checkout, its git common dir, or one of its linked worktrees. When the
//!    probe cannot answer (no `/proc`, `lsof` missing or erroring, an
//!    unreadable git process), the checkout is skipped too.
//!
//! The git dir is resolved with `git rev-parse --git-common-dir`, never by
//! assuming `<repo>/.git`, so linked worktrees share the one pass over their
//! common object store. Nothing here runs `git gc`.
//!
//! # Cadence and callers
//!
//! - The scheduled reaper tier ([`crate::worktree_reaper::reap_repo`]) and the
//!   below-floor eager tier ([`crate::eager_reclaim::run_for`]) call
//!   [`run_for`], which honors this pass's own per-repo cooldown.
//! - `loom-daemon clean` calls [`clean_section`], which ignores the cooldown
//!   and honors `--dry-run`.
//!
//! Every pass that finds something logs one record carrying
//! `category=git_tmp_pack` ([`GIT_TMP_PACK_CATEGORY`]) with the file count and
//! bytes freed, so reclaims are attributable in SigNoz (#10985).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};

// ============================================================================
// Constants
// ============================================================================

/// The reclaim category this pass reports under (#10985). One named constant
/// so a future shared reclaim-telemetry helper can adopt it unchanged.
pub const GIT_TMP_PACK_CATEGORY: &str = "git_tmp_pack";

/// Master on/off env override. Default-on: `0`/`false`/`no`/`off` disables,
/// `1`/`true`/`yes`/`on` force-enables even when config disables it.
pub const GIT_TMP_RECLAIM_ENABLE_ENV: &str = "LOOM_GIT_TMP_RECLAIM";

/// Env override for the grace period (minutes).
pub const GIT_TMP_RECLAIM_GRACE_MINUTES_ENV: &str = "LOOM_GIT_TMP_RECLAIM_GRACE_MINUTES";

/// Env override for the cooldown between scheduled passes (seconds).
pub const GIT_TMP_RECLAIM_MIN_INTERVAL_ENV: &str = "LOOM_GIT_TMP_RECLAIM_MIN_INTERVAL_SECS";

/// Default grace period: 60 minutes. A fetch in flight rewrites its temp file
/// continuously; one that has not been written for an hour is not running.
pub const DEFAULT_GRACE_MINUTES: u64 = 60;

/// Floor for the grace period, whatever the config says: 5 minutes. A `0`
/// would make the age gate meaningless and leave the liveness probe as the
/// only thing between this pass and a fetch in flight.
pub const MIN_GRACE_MINUTES: u64 = 5;

/// Default cooldown between scheduled passes for one repo: 10 minutes —
/// shorter than the reaper's 15-minute cadence so every reaper tick can run
/// it, long enough that the eager tier's 60s loop cannot re-walk
/// `objects/??` every minute.
pub const DEFAULT_MIN_INTERVAL_SECS: u64 = 600;

const GIT_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(not(target_os = "linux"))]
const LSOF_TIMEOUT: Duration = Duration::from_secs(20);

// ============================================================================
// Config (.loom/config.json → autonomous.worktreeReaper.gitTmpReclaim)
// ============================================================================

/// `.loom/config.json → autonomous.worktreeReaper.gitTmpReclaim`. Every field
/// is `Option` so an absent key falls through to env / default resolution —
/// precedence **env > config > default**, as in [`crate::scratch_reclaim`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitTmpReclaimConfig {
    /// `…gitTmpReclaim.enabled` (default **true**).
    pub enabled: Option<bool>,
    /// `…gitTmpReclaim.graceMinutes` — reclaim temp files at least this old.
    pub grace_minutes: Option<u64>,
    /// `…gitTmpReclaim.minIntervalSecs` — cooldown between scheduled passes.
    pub min_interval_secs: Option<u64>,
}

/// Read the config block, soft-failing every field to `None`.
#[must_use]
pub fn read_config(repo_root: &Path) -> GitTmpReclaimConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) =
        crate::config_resolver::get_path(&effective, "autonomous.worktreeReaper.gitTmpReclaim")
    else {
        return GitTmpReclaimConfig::default();
    };
    GitTmpReclaimConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        grace_minutes: block
            .get("graceMinutes")
            .and_then(serde_json::Value::as_u64),
        min_interval_secs: block
            .get("minIntervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve whether the pass runs — **env > config > default(true)**.
#[must_use]
pub fn resolve_enabled(config: &GitTmpReclaimConfig) -> bool {
    if let Ok(v) = std::env::var(GIT_TMP_RECLAIM_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// Resolve the grace period (minutes) — **env > config > default**, never
/// below [`MIN_GRACE_MINUTES`].
#[must_use]
pub fn resolve_grace_minutes(config: &GitTmpReclaimConfig) -> u64 {
    std::env::var(GIT_TMP_RECLAIM_GRACE_MINUTES_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(config.grace_minutes)
        .unwrap_or(DEFAULT_GRACE_MINUTES)
        .max(MIN_GRACE_MINUTES)
}

/// Resolve the cooldown (seconds) — **env > config > default**. A zero or
/// unparseable env value falls through rather than disabling the gate.
#[must_use]
pub fn resolve_min_interval_secs(config: &GitTmpReclaimConfig) -> u64 {
    std::env::var(GIT_TMP_RECLAIM_MIN_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.min_interval_secs)
        .unwrap_or(DEFAULT_MIN_INTERVAL_SECS)
}

// ============================================================================
// Pure gates
// ============================================================================

/// Whether a file whose mtime is `age_secs` old is past the `grace_secs`
/// grace period. A negative age (clock skew, future mtime) never is.
#[must_use]
pub fn is_reclaimable_age(age_secs: i64, grace_secs: i64) -> bool {
    age_secs >= 0 && age_secs >= grace_secs.max(0)
}

fn is_hex_fanout(name: &str) -> bool {
    name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn has_prefix_and_suffix(name: &str, prefix: &str) -> bool {
    name.len() > prefix.len() && name.starts_with(prefix)
}

/// Whether `path` has exactly one of the two shapes this pass may touch,
/// relative to `objects_dir`:
///
/// - `pack/tmp_pack_<something>`
/// - `<two hex digits>/tmp_obj_<something>`
///
/// Purely lexical; [`sweep`] separately requires the entry to be a regular
/// file (never a symlink or directory).
#[must_use]
pub fn is_tmp_debris_path(objects_dir: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(objects_dir) else {
        return false;
    };
    let parts: Vec<&str> = rel
        .components()
        .map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default();
    match parts.as_slice() {
        ["pack", name] => has_prefix_and_suffix(name, "tmp_pack_"),
        [dir, name] if is_hex_fanout(dir) => has_prefix_and_suffix(name, "tmp_obj_"),
        _ => false,
    }
}

// ============================================================================
// Resolution: the git common dir and the roots the liveness probe watches
// ============================================================================

/// `<git-common-dir>` for `repo_root`, absolute. `None` when `git` fails or
/// the directory does not exist — the caller skips the repo.
#[must_use]
pub fn resolve_common_dir(repo_root: &Path) -> Option<PathBuf> {
    let out =
        crate::cmd_out::run("git", &["rev-parse", "--git-common-dir"], repo_root, GIT_TIMEOUT);
    let output = out.ok_output()?;
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    let abs = if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    };
    abs.canonicalize().ok().filter(|p| p.is_dir())
}

/// The directories a live `git` process may be working in for this object
/// store: the checkout, the common dir, and every linked worktree recorded in
/// `<common>/worktrees/*/gitdir`. Canonicalized where possible so they compare
/// against the real paths `/proc` and `lsof` report.
#[must_use]
pub fn liveness_roots(repo_root: &Path, common_dir: &Path) -> Vec<PathBuf> {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let mut roots = vec![canon(repo_root), canon(common_dir)];
    if let Ok(entries) = std::fs::read_dir(common_dir.join("worktrees")) {
        for entry in entries.flatten() {
            let Ok(gitdir) = std::fs::read_to_string(entry.path().join("gitdir")) else {
                continue;
            };
            // `gitdir` names `<worktree>/.git`; the worktree is its parent.
            if let Some(wt) = Path::new(gitdir.trim()).parent() {
                roots.push(canon(wt));
            }
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn within_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|r| path.starts_with(r))
}

// ============================================================================
// Liveness
// ============================================================================

/// Whether a `git` process may be using this object store right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitLiveness {
    /// No `git` process has a cwd or open file under any watched root.
    Idle,
    /// One does; the string names it. The repo is skipped.
    Busy(String),
    /// The probe could not answer. The repo is skipped (fail closed).
    Unknown(String),
}

/// Parse `lsof -F pcfn` output: does any process other than `self_pid` report
/// a name (`n` line: cwd or open file) under one of `roots`? Pure, so it is
/// testable on every platform.
#[must_use]
pub fn parse_lsof_for_roots(stdout: &str, roots: &[PathBuf], self_pid: u32) -> Option<String> {
    let mut pid: Option<u32> = None;
    let mut comm = String::new();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            pid = rest.parse().ok();
            comm.clear();
        } else if let Some(rest) = line.strip_prefix('c') {
            comm = rest.to_string();
        } else if let Some(rest) = line.strip_prefix('n') {
            let Some(p) = pid else { continue };
            if p != self_pid && within_any(Path::new(rest), roots) {
                return Some(format!("pid {p} ({comm}) is using {rest}"));
            }
        }
    }
    None
}

/// Probe for a live `git` process using any of `roots`.
///
/// Linux walks `/proc`: every process whose `comm` starts with `git` has its
/// `cwd` and every `fd/*` link checked. A git process whose cwd cannot be read
/// while it still exists (another user's, say) makes the answer
/// [`GitLiveness::Unknown`]. Elsewhere it asks `lsof -c git` (processes whose
/// command starts with `git`, all of their files), which is cheap because it
/// never walks the repo tree the way `lsof +D` would.
#[must_use]
pub fn probe_git_liveness(roots: &[PathBuf]) -> GitLiveness {
    probe_platform(roots)
}

#[cfg(target_os = "linux")]
fn probe_platform(roots: &[PathBuf]) -> GitLiveness {
    let self_pid = std::process::id();
    let entries = match std::fs::read_dir("/proc") {
        Ok(e) => e,
        Err(e) => return GitLiveness::Unknown(format!("cannot read /proc: {e}")),
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let dir = entry.path();
        let Ok(comm) = std::fs::read_to_string(dir.join("comm")) else {
            continue; // exited between read_dir and here
        };
        let comm = comm.trim();
        if !comm.starts_with("git") {
            continue;
        }
        match std::fs::read_link(dir.join("cwd")) {
            Ok(cwd) if within_any(&cwd, roots) => {
                return GitLiveness::Busy(format!("pid {pid} ({comm}) has cwd {}", cwd.display()));
            }
            Ok(_) => {}
            Err(e) => {
                if dir.exists() {
                    return GitLiveness::Unknown(format!(
                        "cannot read cwd of git pid {pid} ({comm}): {e}"
                    ));
                }
                continue;
            }
        }
        match std::fs::read_dir(dir.join("fd")) {
            Ok(fds) => {
                for fd in fds.flatten() {
                    if let Ok(target) = std::fs::read_link(fd.path()) {
                        if within_any(&target, roots) {
                            return GitLiveness::Busy(format!(
                                "pid {pid} ({comm}) has {} open",
                                target.display()
                            ));
                        }
                    }
                }
            }
            Err(e) => {
                if dir.exists() {
                    return GitLiveness::Unknown(format!(
                        "cannot read open files of git pid {pid} ({comm}): {e}"
                    ));
                }
            }
        }
    }
    GitLiveness::Idle
}

#[cfg(not(target_os = "linux"))]
fn probe_platform(roots: &[PathBuf]) -> GitLiveness {
    use crate::cmd_out::CmdOutcome;
    let outcome = crate::cmd_out::run(
        "lsof",
        &["-w", "-n", "-P", "-c", "git", "-F", "pcfn"],
        Path::new("/"),
        LSOF_TIMEOUT,
    );
    let output = match outcome {
        CmdOutcome::Ran(o) => o,
        CmdOutcome::Unavailable(u) => {
            return GitLiveness::Unknown(format!("lsof unavailable: {u:?}"));
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(why) = parse_lsof_for_roots(&stdout, roots, std::process::id()) {
        return GitLiveness::Busy(why);
    }
    // `lsof -c git` exits 1 with no output at all when no process matches;
    // anything on stderr alongside a failure means it did not get to answer.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() || stderr.trim().is_empty() {
        GitLiveness::Idle
    } else {
        GitLiveness::Unknown(format!("lsof failed: {}", stderr.trim()))
    }
}

// ============================================================================
// Sweep
// ============================================================================

fn age_secs(meta: &std::fs::Metadata, now: DateTime<Utc>) -> Option<i64> {
    let modified: DateTime<Utc> = meta.modified().ok()?.into();
    Some((now - modified).num_seconds())
}

/// Every debris candidate under `objects_dir` regardless of age: regular
/// files (never symlinks) of the two permitted shapes.
fn candidates(objects_dir: &Path) -> Vec<(PathBuf, std::fs::Metadata)> {
    let mut out = Vec::new();
    let mut scan = |dir: PathBuf| {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // `symlink_metadata`: a symlink is never followed, never removed.
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_file() && is_tmp_debris_path(objects_dir, &path) {
                out.push((path, meta));
            }
        }
    };
    scan(objects_dir.join("pack"));
    if let Ok(entries) = std::fs::read_dir(objects_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let is_real_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_real_dir && name.to_str().is_some_and(is_hex_fanout) {
                scan(entry.path());
            }
        }
    }
    out
}

/// What one sweep found and (unless dry-run) removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepTotals {
    /// Files removed (or that would be, under dry-run).
    pub files: usize,
    /// Bytes freed (or that would be).
    pub bytes: u64,
    /// Candidates younger than the grace period, kept.
    pub kept_young: usize,
}

/// Remove every debris file under `<common_dir>/objects` at least
/// `grace_secs` old. `now` is injected so tests can run in compressed time.
/// Callers must have checked liveness first ([`run_pass`] does).
#[must_use]
pub fn sweep(common_dir: &Path, grace_secs: i64, now: DateTime<Utc>, dry_run: bool) -> SweepTotals {
    let objects_dir = common_dir.join("objects");
    let mut totals = SweepTotals::default();
    // A symlinked `objects/` is an unusual layout this pass does not reason
    // about; leave it alone entirely.
    if !std::fs::symlink_metadata(&objects_dir).is_ok_and(|m| m.file_type().is_dir()) {
        return totals;
    }
    for (path, meta) in candidates(&objects_dir) {
        let Some(age) = age_secs(&meta, now) else {
            continue;
        };
        if !is_reclaimable_age(age, grace_secs) {
            totals.kept_young += 1;
            continue;
        }
        // Defensive re-assertion of the shape gate right before removal.
        if !is_tmp_debris_path(&objects_dir, &path) {
            continue;
        }
        if dry_run {
            totals.files += 1;
            totals.bytes += meta.len();
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                totals.files += 1;
                totals.bytes += meta.len();
            }
            Err(e) => log::debug!("git_tmp_reclaim: could not remove {}: {e}", path.display()),
        }
    }
    totals
}

// ============================================================================
// The pass
// ============================================================================

/// One pass's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitTmpReclaimReport {
    /// The checkout whose object store was evaluated.
    pub repo_root: PathBuf,
    /// Whether the pass was enabled.
    pub enabled: bool,
    /// Whether this was a dry run (nothing removed).
    pub dry_run: bool,
    /// What the sweep found.
    pub totals: SweepTotals,
    /// `Some(reason)` when the sweep did not run: cooldown, unresolvable git
    /// dir, live git process, or a liveness probe that could not answer.
    pub skipped: Option<String>,
    /// When this evaluation ran.
    pub at: DateTime<Utc>,
}

impl GitTmpReclaimReport {
    /// Human-readable freed size, e.g. `"35.1G"` or `"nothing"`.
    #[must_use]
    pub fn removed_human(&self) -> String {
        if self.totals.files == 0 {
            return "nothing".to_string();
        }
        human_size(self.totals.bytes)
    }

    /// The structured log line. Carries `category=` so the record can be
    /// attributed by category in SigNoz (#10985).
    #[must_use]
    pub fn log_line(&self) -> String {
        if let Some(reason) = &self.skipped {
            return format!(
                "git_tmp_reclaim: category={GIT_TMP_PACK_CATEGORY} repo={} skipped: {reason}",
                self.repo_root.display()
            );
        }
        format!(
            "git_tmp_reclaim: category={GIT_TMP_PACK_CATEGORY} repo={} {} files={} \
             bytes_freed={} ({}) kept_young={}",
            self.repo_root.display(),
            if self.dry_run {
                "would_remove"
            } else {
                "removed"
            },
            self.totals.files,
            if self.dry_run { 0 } else { self.totals.bytes },
            human_size(self.totals.bytes),
            self.totals.kept_young,
        )
    }
}

fn human_size(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b >= K * K * K {
        format!("{:.1}G", b / (K * K * K))
    } else if b >= K * K {
        format!("{:.1}M", b / (K * K))
    } else if b >= K {
        format!("{:.1}K", b / K)
    } else {
        format!("{bytes}B")
    }
}

/// Log a pass: `info` when something was (or would be) removed, `warn` when a
/// liveness probe failed to answer (an operator should see that a full disk
/// is not being reclaimed and why), `debug` otherwise.
pub fn log_report(report: &GitTmpReclaimReport) {
    if !report.enabled {
        log::debug!("git_tmp_reclaim: {} disabled", report.repo_root.display());
    } else if report
        .skipped
        .as_deref()
        .is_some_and(|s| s.starts_with("liveness probe"))
    {
        log::warn!("{}", report.log_line());
    } else if report.skipped.is_none() && report.totals.files > 0 {
        log::info!("{}", report.log_line());
    } else {
        log::debug!("{}", report.log_line());
    }
}

/// Everything [`run_pass`] needs besides its seams.
#[derive(Debug, Clone, Copy)]
pub struct PassInputs {
    /// Resolved `enabled`.
    pub enabled: bool,
    /// Resolved grace period, seconds.
    pub grace_secs: i64,
    /// Report without removing.
    pub dry_run: bool,
    /// Evaluation time.
    pub now: DateTime<Utc>,
}

/// One pass over `repo_root` with injected seams: resolve the common dir,
/// probe liveness over its roots, then sweep. Any seam that cannot answer
/// skips the repo.
#[must_use]
pub fn run_pass(
    repo_root: &Path,
    inputs: &PassInputs,
    resolve: &dyn Fn(&Path) -> Option<PathBuf>,
    liveness: &dyn Fn(&[PathBuf]) -> GitLiveness,
) -> GitTmpReclaimReport {
    let report = |totals: SweepTotals, skipped: Option<String>| GitTmpReclaimReport {
        repo_root: repo_root.to_path_buf(),
        enabled: inputs.enabled,
        dry_run: inputs.dry_run,
        totals,
        skipped,
        at: inputs.now,
    };
    if !inputs.enabled {
        return report(SweepTotals::default(), Some("disabled".to_string()));
    }
    let Some(common_dir) = resolve(repo_root) else {
        return report(
            SweepTotals::default(),
            Some("could not resolve `git rev-parse --git-common-dir`".to_string()),
        );
    };
    match liveness(&liveness_roots(repo_root, &common_dir)) {
        GitLiveness::Idle => {}
        GitLiveness::Busy(why) => {
            return report(SweepTotals::default(), Some(format!("live git process: {why}")));
        }
        GitLiveness::Unknown(why) => {
            return report(
                SweepTotals::default(),
                Some(format!("liveness probe could not answer ({why}); failing closed")),
            );
        }
    }
    let totals = sweep(&common_dir, inputs.grace_secs, inputs.now, inputs.dry_run);
    report(totals, None)
}

fn production_inputs(repo_root: &Path, dry_run: bool, now: DateTime<Utc>) -> PassInputs {
    let config = read_config(repo_root);
    PassInputs {
        enabled: resolve_enabled(&config),
        grace_secs: i64::try_from(resolve_grace_minutes(&config).saturating_mul(60))
            .unwrap_or(i64::MAX),
        dry_run,
        now,
    }
}

// ============================================================================
// Process-global per-repo cooldown (scheduled + eager tiers only)
// ============================================================================

static LAST_RUN_AT: OnceLock<Mutex<BTreeMap<PathBuf, DateTime<Utc>>>> = OnceLock::new();

fn last_run_slot() -> &'static Mutex<BTreeMap<PathBuf, DateTime<Utc>>> {
    LAST_RUN_AT.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn last_run_at(repo_root: &Path) -> Option<DateTime<Utc>> {
    last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(repo_root)
        .copied()
}

fn record_run(repo_root: &Path, now: DateTime<Utc>) {
    last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(repo_root.to_path_buf(), now);
}

/// Drop cooldown state. Test-only seam.
#[doc(hidden)]
pub fn reset_state_for_test() {
    last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// Whether a pass that last ran at `last` is still inside its cooldown.
#[must_use]
pub fn in_cooldown(
    last: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    min_interval_secs: u64,
) -> bool {
    last.is_some_and(|last| {
        let since = (now - last).num_seconds();
        since >= 0 && since < i64::try_from(min_interval_secs).unwrap_or(i64::MAX)
    })
}

/// Production pass for the scheduled reaper and the eager below-floor tier:
/// honors this pass's own per-repo cooldown, then logs.
pub fn run_for(repo_root: &Path) -> GitTmpReclaimReport {
    let now = Utc::now();
    let inputs = production_inputs(repo_root, false, now);
    let min_interval = resolve_min_interval_secs(&read_config(repo_root));
    let report = if inputs.enabled && in_cooldown(last_run_at(repo_root), now, min_interval) {
        GitTmpReclaimReport {
            repo_root: repo_root.to_path_buf(),
            enabled: true,
            dry_run: false,
            totals: SweepTotals::default(),
            skipped: Some(format!("cooldown ({min_interval}s) has not elapsed")),
            at: now,
        }
    } else {
        let r = run_pass(repo_root, &inputs, &resolve_common_dir, &probe_git_liveness);
        if r.enabled {
            record_run(repo_root, now);
        }
        r
    };
    log_report(&report);
    report
}

/// The `loom-daemon clean` section: no cooldown, honors `--dry-run`, prints
/// one line for the operator and logs the structured record.
pub fn clean_section(repo_root: &Path, dry_run: bool) -> GitTmpReclaimReport {
    let inputs = production_inputs(repo_root, dry_run, Utc::now());
    let report = run_pass(repo_root, &inputs, &resolve_common_dir, &probe_git_liveness);
    log_report(&report);
    match (&report.skipped, dry_run) {
        (Some(reason), _) => println!("Skipped: {reason}"),
        (None, _) if report.totals.files == 0 => {
            println!("No aborted-fetch temp files (tmp_pack_*/tmp_obj_*) past the grace period");
        }
        (None, true) => println!(
            "Would remove {} aborted-fetch temp file(s) ({})",
            report.totals.files,
            human_size(report.totals.bytes)
        ),
        (None, false) => println!(
            "Removed {} aborted-fetch temp file(s) ({})",
            report.totals.files,
            report.removed_human()
        ),
    }
    report
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "git_tmp_reclaim/tests.rs"]
mod tests;
