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
//!   `<repo>/.loom/target-*`, and agent-shaped names ([`is_agent_shaped`])
//!   under `/tmp`, `$TMPDIR` and `~/.cache`: `cargo-target-issue-<N>`,
//!   `loom-target-<N>-doctor`. One fleet host held 85 GB of
//!   `.loom/target-{builder,doctor,judge}-<N>`; another 45 GB of
//!   `/private/tmp/cargo-target-{issue,review}-*`. A bare prefix is not
//!   enough outside the repo: a human's `~/.cache/cargo-target-shared` is
//!   theirs.
//!
//! Out of scope: Claude Code session scratchpads and `$TMPDIR/opencode/*`
//! clones. Neither has a stable prefix this module could match without also
//! matching things that are not build output.
//!
//! # Safety: every gate must pass, and an unknown is a keep
//!
//! A candidate is removed only when ALL of these hold:
//!
//! 0. Its scan root is trusted ([`vet_root`]). A root inside the repo
//!    (`.loom`, `.loom/targets`) is scanned only when every path component
//!    below the repo root is a real directory, not a symlink, and its
//!    canonical path is inside the canonical repo root. A `.loom/targets`
//!    symlinked onto a bigger volume (or at `$HOME`) is refused whole:
//!    its children are somebody else's directories.
//! 1. It is a direct child of one of the scan roots and its name matches that
//!    root's matcher. Nothing is found by walking deeper.
//! 2. It is a real directory, not a symlink (`symlink_metadata`), so a link
//!    named `cargo-target-x` pointing at `$HOME` is never followed.
//! 2a. Under `.loom/targets` it carries the owner marker
//!    ([`crate::run_target_dir::OWNER_FILE`]) that `provision` writes. A
//!    directory Loom did not create there is not Loom's to remove.
//! 2b. Under a shared root (`/tmp`, `$TMPDIR`, `~/.cache`) it is owned by
//!    this process's effective uid. Liveness cannot be established for
//!    another user's directory (`/proc/<pid>/fd` and `lsof` do not show
//!    another user's processes to a non-root caller), so it is kept.
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

/// Name prefix for the improvised location inside the repo's `.loom/`.
pub const LEGACY_REPO_PREFIX: &str = "target-";
/// Prefixes matched directly under `/tmp` (with an agent-shaped suffix).
pub const TMP_PREFIXES: &[&str] = &["loom-target-", "cargo-target-"];
/// Prefix matched directly under `$TMPDIR` and `~/.cache` (same rule).
pub const CARGO_TARGET_PREFIX: &str = "cargo-target-";
/// The words an agent put in an improvised name: its role, or what it was
/// working on.
pub const AGENT_NAME_WORDS: &[&str] = &["issue", "review", "pr", "builder", "doctor", "judge"];

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
    pub scope: RootScope,
}

/// Where a [`ScanRoot`] lives, which decides how the root itself is vetted
/// and which ownership gate its children pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootScope {
    /// Inside the repo (`.loom`, `.loom/targets`). The root must be a real
    /// directory chain under the repo root ([`vet_root`]).
    Repo,
    /// A directory other users and other tools also write to (`/tmp`,
    /// `$TMPDIR`, `~/.cache`). A child must be owned by this process's euid.
    Shared,
}

/// Which child names under a [`ScanRoot`] are candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameMatcher {
    /// `<repo>/.loom/targets/`: a `<role>-<run-id>` name. The name alone is
    /// never enough there; [`evaluate`] also requires the owner marker.
    RunDir,
    /// Any of these prefixes, followed by at least one more character. Only
    /// for a root inside the repo.
    Prefixes(Vec<&'static str>),
    /// Any of these prefixes followed by an agent-shaped suffix
    /// ([`is_agent_shaped`]). For the shared roots.
    AgentShaped(Vec<&'static str>),
}

/// Whether `suffix` (what follows a `cargo-target-` / `loom-target-` prefix)
/// is a name an agent improvised: `-`-separated tokens, each one of
/// [`AGENT_NAME_WORDS`] or a run of digits, with at least one of each.
/// `issue-10078`, `review-9745`, `10570-doctor` are; `shared`, `x`, `1`,
/// `issue`, `my-issue-3` are not, so a human's
/// `~/.cache/cargo-target-shared` never matches.
#[must_use]
pub fn is_agent_shaped(suffix: &str) -> bool {
    let (mut words, mut numbers) = (0usize, 0usize);
    for token in suffix.split('-') {
        if AGENT_NAME_WORDS.contains(&token) {
            words += 1;
        } else if !token.is_empty()
            && token.len() <= 10
            && token.bytes().all(|b| b.is_ascii_digit())
        {
            numbers += 1;
        } else {
            return false;
        }
    }
    words >= 1 && numbers >= 1
}

/// Whether `name` has the `<role>-<run-id>` shape `planned_for` produces:
/// `[A-Za-z0-9_-]+`, starting with an alphanumeric, with a `-` separating a
/// non-empty role from a non-empty run id.
fn is_run_dir_name(name: &str) -> bool {
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .split_once('-')
            .is_some_and(|(role, id)| !role.is_empty() && !id.trim_matches('-').is_empty())
}

impl NameMatcher {
    #[must_use]
    pub fn matches(&self, name: &str) -> bool {
        match self {
            Self::RunDir => is_run_dir_name(name),
            Self::Prefixes(prefixes) => prefixes
                .iter()
                .any(|p| name.len() > p.len() && name.starts_with(p)),
            Self::AgentShaped(prefixes) => prefixes
                .iter()
                .any(|p| name.strip_prefix(p).is_some_and(is_agent_shaped)),
        }
    }
}

impl ScanRoot {
    /// The per-candidate gates that depend on which root a candidate is in.
    #[must_use]
    pub fn gates(&self) -> RootGates {
        RootGates {
            require_owner_marker: self.matcher == NameMatcher::RunDir,
            require_own_uid: self.scope == RootScope::Shared,
        }
    }
}

/// The root-dependent gates [`evaluate`] applies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RootGates {
    /// Keep a dir with no [`crate::run_target_dir::OWNER_FILE`] marker.
    pub require_owner_marker: bool,
    /// Keep a dir whose owner uid is not this process's euid.
    pub require_own_uid: bool,
}

/// Whether `root` may be scanned at all. A shared root always may (its
/// children pass the ownership gate instead). A root inside the repo may only
/// when every path component from the repo root down to it is a real
/// directory (`symlink_metadata`, so a symlink at ANY level refuses it, not
/// just the last) and its canonical path is inside the canonical repo root.
/// `Err` carries why; a root that does not exist is `Err` too, and silent.
pub fn vet_root(repo_root: &Path, root: &ScanRoot) -> Result<(), String> {
    if root.scope == RootScope::Shared {
        return Ok(());
    }
    vet_path_inside(repo_root, &root.dir)
}

/// [`vet_root`]'s check for a root inside the repo, for any `dir` under any
/// `base`: every component below `base` is a real directory (a symlink at any
/// level refuses it) and `dir`'s canonical path is strictly inside `base`'s.
/// Also how the kept-worktree artifact reclaim vets `<worktree>/target`
/// (#11071).
pub fn vet_path_inside(repo_root: &Path, dir: &Path) -> Result<(), String> {
    let Ok(relative) = dir.strip_prefix(repo_root) else {
        return Err(format!("not under the repo root {}", repo_root.display()));
    };
    let mut walked = repo_root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(format!("non-normal path component in {}", relative.display()));
        };
        walked.push(part);
        match std::fs::symlink_metadata(&walked) {
            Ok(meta) if meta.is_dir() => {}
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!("{} is a symlink", walked.display()));
            }
            Ok(_) => return Err(format!("{} is not a directory", walked.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(MISSING_ROOT.to_string());
            }
            Err(e) => return Err(format!("{}: {e}", walked.display())),
        }
    }
    let (Ok(real_root), Ok(real_repo)) = (dir.canonicalize(), repo_root.canonicalize()) else {
        return Err("could not canonicalize the root or the repo root".to_string());
    };
    if real_root == real_repo || !real_root.starts_with(&real_repo) {
        return Err(format!(
            "resolves to {}, outside the repo root {}",
            real_root.display(),
            real_repo.display()
        ));
    }
    Ok(())
}

/// [`vet_root`]'s reason for a root that simply is not there (the common
/// case: no run has created `.loom/targets` yet). Not worth a log line.
const MISSING_ROOT: &str = "does not exist";

/// The fixed scan roots for `repo_root`, with `home` and `tmpdir` injected.
/// A root that resolves to the same real directory as an earlier one (macOS
/// `$TMPDIR` is never `/tmp`, but a Linux one often is) is merged into it.
#[must_use]
pub fn scan_roots(repo_root: &Path, home: Option<&Path>, tmpdir: Option<&Path>) -> Vec<ScanRoot> {
    let mut roots = vec![
        ScanRoot {
            dir: crate::run_target_dir::targets_root(repo_root),
            matcher: NameMatcher::RunDir,
            scope: RootScope::Repo,
        },
        ScanRoot {
            dir: repo_root.join(".loom"),
            matcher: NameMatcher::Prefixes(vec![LEGACY_REPO_PREFIX]),
            scope: RootScope::Repo,
        },
        ScanRoot {
            dir: PathBuf::from("/tmp"),
            matcher: NameMatcher::AgentShaped(TMP_PREFIXES.to_vec()),
            scope: RootScope::Shared,
        },
    ];
    if let Some(tmpdir) = tmpdir.filter(|p| p.is_absolute()) {
        roots.push(ScanRoot {
            dir: tmpdir.to_path_buf(),
            matcher: NameMatcher::AgentShaped(vec![CARGO_TARGET_PREFIX]),
            scope: RootScope::Shared,
        });
    }
    if let Some(home) = home.filter(|p| p.is_absolute()) {
        roots.push(ScanRoot {
            dir: home.join(".cache"),
            matcher: NameMatcher::AgentShaped(vec![CARGO_TARGET_PREFIX]),
            scope: RootScope::Shared,
        });
    }
    let mut merged: Vec<(PathBuf, ScanRoot)> = Vec::new();
    for root in roots {
        let real = crate::worktree_ops::cargo_target::realish(&root.dir);
        if let Some((_, existing)) = merged.iter_mut().find(|(r, _)| *r == real) {
            // Only two shared roots ever merge (a `$TMPDIR` that is `/tmp`).
            // A shared root that resolves to a repo root is dropped: the
            // stricter repo vetting already covers that directory.
            if let (NameMatcher::AgentShaped(have), NameMatcher::AgentShaped(add)) =
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
    /// Under `.loom/targets` without the owner marker `provision` writes:
    /// Loom did not create it.
    NoOwnerMarker,
    /// Under a shared root and owned by another user.
    ForeignOwner { uid: u32 },
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
    /// This process's effective uid, for the shared-root ownership gate.
    pub euid: u32,
}

/// This process's effective uid.
#[must_use]
pub fn current_euid() -> u32 {
    // SAFETY: geteuid only reads the caller's effective user id.
    unsafe { libc::geteuid() }
}

/// Whether a directory owned by `owner_uid` may be removed from a shared
/// root by a process running as `euid`: only its own. Root gets no pass: a
/// root daemon is exactly the case where another user's dir would otherwise
/// be deletable.
#[must_use]
pub fn owned_by_us(owner_uid: u32, euid: u32) -> bool {
    owner_uid == euid
}

/// Whether `dir` carries a real (non-symlink) owner marker file with a pid in
/// it, as [`crate::run_target_dir::provision`] writes.
fn has_owner_marker(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir.join(crate::run_target_dir::OWNER_FILE))
        .is_ok_and(|m| m.is_file())
        && crate::run_target_dir::owner_pid(dir).is_some()
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
pub(crate) fn newest_mtime_and_size(path: &Path) -> std::io::Result<(DateTime<Utc>, u64)> {
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
    gates: RootGates,
    now: DateTime<Utc>,
    max_age_secs: i64,
    probes: &Probes<'_>,
) -> Result<Candidate, KeepReason> {
    use std::os::unix::fs::MetadataExt;

    let meta = std::fs::symlink_metadata(path).map_err(|_| KeepReason::Unreadable)?;
    if !meta.is_dir() {
        return Err(KeepReason::NotADirectory);
    }
    if gates.require_own_uid && !owned_by_us(meta.uid(), probes.euid) {
        return Err(KeepReason::ForeignOwner { uid: meta.uid() });
    }
    if gates.require_owner_marker && !has_owner_marker(path) {
        return Err(KeepReason::NoOwnerMarker);
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
    /// Scan roots that were not scanned at all, with why ([`vet_root`]): a
    /// symlinked `.loom/targets`, a root that resolves outside the repo.
    pub refused_roots: Vec<(PathBuf, String)>,
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
    for (root, why) in &report.refused_roots {
        log::warn!(
            "target_orphan_reclaim: category={CATEGORY} not scanning {}: {why}; nothing under \
             it will be reclaimed",
            root.display()
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
        if let Err(why) = vet_root(repo_root, root) {
            if why != MISSING_ROOT {
                report.refused_roots.push((root.dir.clone(), why));
            }
            continue;
        }
        let gates = root.gates();
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
            match evaluate(&path, gates, now, max_age_secs, probes) {
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
pub(crate) fn production_open_handles(path: &Path) -> Option<bool> {
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
        roots.retain(|r| r.scope == RootScope::Repo);
    }
    let protected = host_configured_target_dirs(repo_root);
    let live_issues = crate::worktree_ops::liveness::active_spawn_loop_issues(repo_root);
    let probes = Probes {
        open_handles: &production_open_handles,
        owner_alive: &crate::live_claim::pid_is_live_process,
        live_issues: &live_issues,
        protected: &protected,
        euid: current_euid(),
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
