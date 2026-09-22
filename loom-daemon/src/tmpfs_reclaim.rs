//! Reclaim of orphaned Loom-named scratch directories parked on a `tmpfs`/
//! `ramfs` mount (issue #8512) — the host-RAM counterpart of
//! [`crate::deep_clean`]'s worktree-`target/` reclaim and
//! [`crate::target_dir_gc`]'s shared cargo-cache prune.
//!
//! # What was leaking
//!
//! Nothing in the daemon has ever been aware of `/dev/shm` or any other
//! `tmpfs`/`ramfs` mount. A build that redirects `CARGO_TARGET_DIR` (or an
//! agent scratch dir) onto `/dev/shm/cargo-target-<issue>` writes RAM, not
//! disk — and once the worktree that created it is removed, nothing can
//! attribute the directory back to a worktree to reclaim it (see
//! `defaults/docs/troubleshooting.md` → "Redirected cargo target dirs are
//! reclaimed with their worktree" (#7239) for why the *attribution*-based
//! reclaim deliberately never deletes by name/pattern match alone). One
//! incident measured a 6.2 GB orphan pinned in RAM for 2.5 days, ending in a
//! kernel OOM-kill storm on unrelated processes — invisible to `df`/`du` over
//! the repo tree, because it never touched disk at all.
//!
//! # A deliberately different, narrower safety model
//!
//! The per-worktree cargo-target reclaim (`crate::worktree_ops::cargo_target`)
//! refuses to act on anything it cannot prove belongs to the worktree being
//! removed — a name/pattern match alone is not enough, because a
//! machine-global redirect resolves identically for every worktree on the
//! host. That model does not fit here: by the time an orphaned tmpfs
//! directory is noticed, its owning worktree is long gone, so there is
//! nothing left to attribute it to. This module trades provable attribution
//! for four independently-checked, narrower signals instead — a recognized
//! Loom scratch-name pattern, a confirmed `tmpfs`/`ramfs` mount type (never a
//! hardcoded `/dev/shm` prefix), no live process holding the directory open
//! (reusing [`crate::worktree_ops::safety::find_processes_using_directory`],
//! the same evidence-based check the worktree reaper itself uses), and an age
//! floor on the directory's newest mtime found anywhere underneath it (never
//! a bare "it exists" check). All four must hold before anything is removed;
//! failing to prove any one of them keeps the directory, the same fail-safe
//! direction every other reclaim pass in this crate takes.
//!
//! # Shape: a `worktree_reaper` sibling pass, not a worktree-keyed reclaim
//!
//! A tmpfs orphan is host-level, not tied to any one worktree's removal —
//! unlike [`crate::worktree_reaper`]'s own per-worktree reclaim passes, this
//! has nothing to key off of. It rides [`crate::worktree_reaper::reap_repo`]
//! as a third sibling pass alongside [`crate::deep_clean::run_for`] and
//! [`crate::docker_image_clean::run_for`] — both already host/machine-scoped
//! passes invoked from that same tick, not the per-worktree
//! `reap_worktrees()` path. Also exposed standalone as
//! `loom-daemon tmpfs-scratch-gc` (mirroring
//! [`crate::target_dir_gc`]'s CLI shape) for a manual or cron-driven run.
//!
//! # Default-on, host-wide cooldown
//!
//! Like [`crate::docker_image_clean`], this is default-on (an unbounded tmpfs
//! leak is not something anyone opts into) and keeps a **host-wide** (not
//! per-repo) cooldown state, since a `tmpfs` mount is not scoped to any one
//! registered repo and a multi-repo host must not re-walk `/proc/mounts` once
//! per repo per reaper tick. Opt out with `LOOM_TMPFS_SCRATCH_GC=0` or
//! `autonomous.tmpfsScratchGc.enabled=false`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

// ============================================================================
// Constants
// ============================================================================

/// Master on/off env override. Default-on: `0`/`false`/`no`/`off` disables,
/// `1`/`true`/`yes`/`on` force-enables even when config disables it.
pub const TMPFS_RECLAIM_ENABLE_ENV: &str = "LOOM_TMPFS_SCRATCH_GC";

/// Env override for the staleness window (seconds).
pub const TMPFS_RECLAIM_STALENESS_ENV: &str = "LOOM_TMPFS_SCRATCH_GC_STALENESS_SECS";

/// Env override for the host-wide cooldown between passes (seconds).
pub const TMPFS_RECLAIM_MIN_INTERVAL_ENV: &str = "LOOM_TMPFS_SCRATCH_GC_MIN_INTERVAL_SECS";

/// Override for which file is read as `/proc/mounts` — production always uses
/// the real path; tests point this at a fixture file so mount classification
/// is exercised without depending on the real host's mount table.
pub const MOUNTS_FILE_ENV: &str = "LOOM_TMPFS_RECLAIM_MOUNTS_FILE";

/// The real, production mount table.
const DEFAULT_MOUNTS_FILE: &str = "/proc/mounts";

/// Default staleness threshold: 6 hours — the figure #8570/#8572 (this
/// issue's own prior splits) already narrate as this module's behavior.
pub const DEFAULT_STALENESS_SECS: u64 = 6 * 3_600;

/// Default cooldown between passes: 30 minutes, matching
/// [`crate::docker_image_clean::DEFAULT_MIN_INTERVAL_SECS`] — cheap but
/// non-zero (a `/proc/mounts` read plus a shallow `read_dir` per tmpfs mount),
/// so the cooldown exists purely to avoid re-walking on every registered repo
/// on a multi-repo host's same reaper tick.
pub const DEFAULT_MIN_INTERVAL_SECS: u64 = 1_800;

/// Loom-named scratch directory patterns this pass recognizes — kept narrow
/// on purpose (see module docs): a name pattern is one of four independent
/// signals required before anything is removed, not sufficient on its own.
pub const DEFAULT_NAME_PATTERNS: &[&str] = &["cargo-target-*", "tmp-issue*"];

/// Filesystem types this pass treats as RAM-backed (never disk).
const RAM_BACKED_FS_TYPES: &[&str] = &["tmpfs", "ramfs"];

// ============================================================================
// Config (.loom/config.json → autonomous.tmpfsScratchGc)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.tmpfsScratchGc` this module
/// consumes. Every field is `Option`, matching every other `autonomous.*`
/// surface's env > config > default precedence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TmpfsReclaimConfig {
    /// `…tmpfsScratchGc.enabled` (default **true**).
    pub enabled: Option<bool>,
    /// `…tmpfsScratchGc.stalenessSecs` (default [`DEFAULT_STALENESS_SECS`]).
    pub staleness_secs: Option<u64>,
    /// `…tmpfsScratchGc.minIntervalSecs` (default
    /// [`DEFAULT_MIN_INTERVAL_SECS`]; a zero/invalid value drops to `None`).
    pub min_interval_secs: Option<u64>,
    /// `…tmpfsScratchGc.namePatterns` — scratch-dir name patterns this pass
    /// recognizes (default [`DEFAULT_NAME_PATTERNS`]).
    pub name_patterns: Option<Vec<String>>,
}

/// Read `.loom/config.json → autonomous.tmpfsScratchGc`, soft-failing every
/// field to `None` (env/default resolution) on a missing file, malformed
/// JSON, or a missing block.
#[must_use]
pub fn read_tmpfs_reclaim_config(repo_root: &Path) -> TmpfsReclaimConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(&effective, "autonomous.tmpfsScratchGc")
    else {
        return TmpfsReclaimConfig::default();
    };

    TmpfsReclaimConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        staleness_secs: block
            .get("stalenessSecs")
            .and_then(serde_json::Value::as_u64),
        min_interval_secs: block
            .get("minIntervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        name_patterns: block
            .get("namePatterns")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            }),
    }
}

/// Resolve whether the pass runs — precedence **env > config > default(true)**.
#[must_use]
pub fn resolve_enabled(config: &TmpfsReclaimConfig) -> bool {
    if let Ok(v) = std::env::var(TMPFS_RECLAIM_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// Resolve the staleness window (seconds) — precedence **env > config >
/// default**.
#[must_use]
pub fn resolve_staleness_secs(config: &TmpfsReclaimConfig) -> u64 {
    std::env::var(TMPFS_RECLAIM_STALENESS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(config.staleness_secs)
        .unwrap_or(DEFAULT_STALENESS_SECS)
}

/// Resolve the host-wide cooldown (seconds) — precedence **env > config >
/// default**.
#[must_use]
pub fn resolve_min_interval_secs(config: &TmpfsReclaimConfig) -> u64 {
    std::env::var(TMPFS_RECLAIM_MIN_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.min_interval_secs)
        .unwrap_or(DEFAULT_MIN_INTERVAL_SECS)
}

/// Resolve the recognized name-pattern list — precedence **config >
/// default** (no env override: this is a set, not a scalar).
#[must_use]
pub fn resolve_name_patterns(config: &TmpfsReclaimConfig) -> Vec<String> {
    config.name_patterns.clone().unwrap_or_else(|| {
        DEFAULT_NAME_PATTERNS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    })
}

// ============================================================================
// Mount classification (`/proc/mounts` parsing — pure, fixture-testable)
// ============================================================================

/// One entry from `/proc/mounts`: a mount point and its filesystem type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub mount_point: PathBuf,
    pub fs_type: String,
}

/// Undo `/proc/mounts`' octal escaping of whitespace/backslash in a mount
/// point (the kernel escapes space, tab, newline, and backslash itself so the
/// whitespace-delimited format stays parseable).
fn unescape_mount_point(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let octal = &raw[i + 1..i + 4];
            if let Ok(code) = u8::from_str_radix(octal, 8) {
                out.push(code as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Parse `/proc/mounts`-format text into every mount entry it describes.
/// Malformed lines (fewer than 3 whitespace-separated fields) are skipped,
/// never treated as a fatal parse error — one corrupt line must not blind the
/// pass to every other, correctly-formed mount.
#[must_use]
pub fn parse_mounts(content: &str) -> Vec<MountEntry> {
    content
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let _device = parts.next()?;
            let mount_point = parts.next()?;
            let fs_type = parts.next()?;
            Some(MountEntry {
                mount_point: PathBuf::from(unescape_mount_point(mount_point)),
                fs_type: fs_type.to_string(),
            })
        })
        .collect()
}

/// Filter parsed mount entries down to the RAM-backed ones (`tmpfs`/`ramfs`)
/// — the pure, fixture-testable core of [`ram_backed_mount_points`].
#[must_use]
pub fn ram_backed_mount_points_from(content: &str) -> Vec<MountEntry> {
    parse_mounts(content)
        .into_iter()
        .filter(|m| RAM_BACKED_FS_TYPES.contains(&m.fs_type.as_str()))
        .collect()
}

/// Every `tmpfs`/`ramfs` mount on the host, read from [`MOUNTS_FILE_ENV`] (or
/// `/proc/mounts` in production). A missing or unreadable mounts file
/// degrades to an empty list — **not** an error — so a host with no readable
/// `/proc/mounts` (a non-Linux daemon host, an unusual sandbox) is a clean
/// no-op rather than a failure: "unmeasurable" here means "nothing to scan",
/// the safe direction for a pass whose only job is finding removal
/// candidates.
#[must_use]
pub fn ram_backed_mount_points() -> Vec<MountEntry> {
    let path = std::env::var(MOUNTS_FILE_ENV).unwrap_or_else(|_| DEFAULT_MOUNTS_FILE.to_string());
    match fs::read_to_string(&path) {
        Ok(content) => ram_backed_mount_points_from(&content),
        Err(_) => Vec::new(),
    }
}

// ============================================================================
// Name-pattern matching (pure)
// ============================================================================

/// Whether `name` matches any of `patterns`. A pattern with no `*` is an
/// exact match; one or more `*` wildcards match any (possibly empty) run of
/// characters — sufficient for the simple prefix-style patterns this module
/// uses (`"cargo-target-*"`, `"tmp-issue*"`) without pulling in a full glob
/// dependency for two fixed shapes.
#[must_use]
pub fn matches_name_patterns(name: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|pattern| matches_pattern(name, pattern))
}

fn matches_pattern(name: &str, pattern: &str) -> bool {
    if !pattern.contains('*') {
        return name == pattern;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = name;

    if let Some(first) = parts.first() {
        if !first.is_empty() {
            let Some(stripped) = rest.strip_prefix(first) else {
                return false;
            };
            rest = stripped;
        }
    }
    if parts.len() > 1 {
        if let Some(last) = parts.last() {
            if !last.is_empty() {
                let Some(stripped) = rest.strip_suffix(last) else {
                    return false;
                };
                rest = stripped;
            }
        }
    }
    for mid in parts.iter().skip(1).take(parts.len().saturating_sub(2)) {
        if mid.is_empty() {
            continue;
        }
        match rest.find(mid) {
            Some(idx) => rest = &rest[idx + mid.len()..],
            None => return false,
        }
    }
    true
}

// ============================================================================
// Candidate model + pure eligibility (mirrors `target_dir_gc::PruneCandidate`)
// ============================================================================

/// One Loom-named scratch directory found directly under a RAM-backed mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchDirCandidate {
    pub path: PathBuf,
    pub mount_point: PathBuf,
    /// Total on-disk (really: in-RAM) size in bytes, recursive.
    pub size_bytes: u64,
    /// The newest mtime found anywhere under `path` — never the directory's
    /// own mtime alone, mirroring [`crate::target_dir_gc`]'s "recursive mtime
    /// freshness" gate so a directory receiving in-place rewrites (no new
    /// directory entries) is not misjudged as stale.
    pub newest_mtime: DateTime<Utc>,
    /// Whether any live process currently holds a file open under `path`
    /// (from [`crate::worktree_ops::safety::find_processes_using_directory`]).
    pub has_open_handle: bool,
}

impl ScratchDirCandidate {
    #[must_use]
    pub fn age_secs(&self, now: DateTime<Utc>) -> i64 {
        (now - self.newest_mtime).num_seconds().max(0)
    }

    /// Whether this candidate may be reclaimed: **no** open handle **and**
    /// at least `staleness_secs` old. Both conditions are required — see the
    /// module docs' four-signal safety model.
    #[must_use]
    pub fn is_eligible(&self, now: DateTime<Utc>, staleness_secs: u64) -> bool {
        if self.has_open_handle {
            return false;
        }
        let age_secs = self.age_secs(now);
        let threshold_secs = i64::try_from(staleness_secs).unwrap_or(i64::MAX);
        age_secs >= threshold_secs
    }
}

/// Render a byte count the way every other reclaim pass's CLI does
/// (`"6.2G"`, `"512K"`, `"0B"`). Public so the `tmpfs-scratch-gc` CLI
/// front-end renders identical figures to the daemon's own log lines instead
/// of keeping a second, drift-prone copy of the same formatter.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    let b = bytes as f64;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}G", b / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}M", b / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}K", b / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

/// The disposition of one scan — every candidate ends up in exactly one
/// bucket. Pure function of `(candidates, now, staleness_secs)`: no I/O,
/// fully unit-testable against a fixture list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReclaimPlan {
    pub remove: Vec<ScratchDirCandidate>,
    pub keep: Vec<ScratchDirCandidate>,
}

impl ReclaimPlan {
    #[must_use]
    pub fn bytes_to_remove(&self) -> u64 {
        self.remove.iter().map(|c| c.size_bytes).sum()
    }

    /// `"2 scratch dir(s) (6.2G)"`, or `"nothing"`.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.remove.is_empty() {
            return "nothing".to_string();
        }
        format!("{} scratch dir(s) ({})", self.remove.len(), human_size(self.bytes_to_remove()))
    }
}

/// Decide what to remove, purely from an already-scanned candidate list —
/// every candidate has already had its open-handle check resolved by
/// [`scan_candidates`]; this only applies the staleness rule on top.
#[must_use]
pub fn plan_reclaim(
    candidates: &[ScratchDirCandidate],
    now: DateTime<Utc>,
    staleness_secs: u64,
) -> ReclaimPlan {
    let mut plan = ReclaimPlan::default();
    for candidate in candidates {
        if candidate.is_eligible(now, staleness_secs) {
            plan.remove.push(candidate.clone());
        } else {
            plan.keep.push(candidate.clone());
        }
    }
    plan
}

// ============================================================================
// I/O: scanning and removal
// ============================================================================

/// Recursively find the newest mtime and cumulative size under `path` —
/// identical discipline to `target_dir_gc`'s own helper of the same shape
/// (kept as a separate copy rather than a shared dependency: the two modules
/// have no other coupling, and this is a handful of lines). Best-effort: an
/// unreadable child is skipped rather than failing the whole walk.
fn newest_mtime_and_size(path: &Path) -> io::Result<(DateTime<Utc>, u64)> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        let mut newest = mtime_of(&meta);
        let mut total = 0u64;
        let entries = match fs::read_dir(path) {
            Ok(e) => e,
            Err(_) => return Ok((newest, total)),
        };
        for entry in entries.flatten() {
            if let Ok((child_newest, child_size)) = newest_mtime_and_size(&entry.path()) {
                if child_newest > newest {
                    newest = child_newest;
                }
                total += child_size;
            }
        }
        Ok((newest, total))
    } else {
        Ok((mtime_of(&meta), meta.len()))
    }
}

fn mtime_of(meta: &fs::Metadata) -> DateTime<Utc> {
    meta.modified()
        .ok()
        .map_or_else(Utc::now, DateTime::<Utc>::from)
}

/// Walk every RAM-backed mount point for Loom-named top-level directories.
/// **Non-recursive into the mount itself** — only direct children of a
/// classified `tmpfs`/`ramfs` mount point are ever considered, so nothing
/// outside a mount this pass already classified as RAM-backed can ever
/// become a candidate, regardless of name.
#[must_use]
pub fn scan_candidates(
    mount_points: &[MountEntry],
    name_patterns: &[String],
) -> Vec<ScratchDirCandidate> {
    let mut candidates = Vec::new();
    for mount in mount_points {
        let Ok(entries) = fs::read_dir(&mount.mount_point) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !matches_name_patterns(&name_str, name_patterns) {
                continue;
            }
            let path = entry.path();
            let (newest_mtime, size_bytes) =
                newest_mtime_and_size(&path).unwrap_or_else(|_| (Utc::now(), 0));
            let has_open_handle =
                !crate::worktree_ops::safety::find_processes_using_directory(&path).is_empty();
            candidates.push(ScratchDirCandidate {
                path,
                mount_point: mount.mount_point.clone(),
                size_bytes,
                newest_mtime,
                has_open_handle,
            });
        }
    }
    candidates
}

/// Remove one candidate directory. A failure is a soft, per-candidate
/// failure — logged and excluded from the report's `removed` list, never
/// fatal to the rest of the pass (mirrors
/// [`crate::docker_image_clean::remove_image`]'s per-image failure handling).
fn remove_candidate(candidate: &ScratchDirCandidate) -> bool {
    match fs::remove_dir_all(&candidate.path) {
        Ok(()) => true,
        Err(e) => {
            log::warn!("tmpfs_reclaim: could not remove {}: {e}", candidate.path.display());
            false
        }
    }
}

// ============================================================================
// The pass
// ============================================================================

/// One reclaim pass's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmpfsReclaimReport {
    /// Whether the pass was enabled for this evaluation.
    pub enabled: bool,
    pub dry_run: bool,
    pub staleness_secs: u64,
    /// `None` when disabled or skipped by the host-wide cooldown.
    pub plan: Option<ReclaimPlan>,
    /// What was **actually** deleted this pass — empty on a dry run or when
    /// every removal attempt failed.
    pub removed: Vec<ScratchDirCandidate>,
    /// Present when the host-wide cooldown skipped this evaluation.
    pub deferred: Option<String>,
    pub at: DateTime<Utc>,
}

impl TmpfsReclaimReport {
    #[must_use]
    pub fn bytes_reclaimed(&self) -> u64 {
        self.removed.iter().map(|c| c.size_bytes).sum()
    }

    #[must_use]
    pub fn bytes_reclaimable(&self) -> u64 {
        self.plan.as_ref().map_or(0, ReclaimPlan::bytes_to_remove)
    }

    /// One-line rationale for logging / CLI output.
    #[must_use]
    pub fn reason(&self) -> String {
        if !self.enabled {
            return "disabled (autonomous.tmpfsScratchGc.enabled=false or \
                     LOOM_TMPFS_SCRATCH_GC unset-falsy)"
                .to_string();
        }
        if let Some(reason) = &self.deferred {
            return format!("deferred: {reason}");
        }
        let Some(plan) = &self.plan else {
            return "no plan computed".to_string();
        };
        if self.dry_run {
            return format!(
                "DRY RUN: would reclaim {} ({} candidates, {} kept)",
                human_size(self.bytes_reclaimable()),
                plan.remove.len(),
                plan.keep.len()
            );
        }
        format!(
            "reclaimed {} of planned {} ({} kept)",
            human_size(self.bytes_reclaimed()),
            human_size(self.bytes_reclaimable()),
            plan.keep.len()
        )
    }
}

/// Run one pass over an **explicitly supplied** mount list — the injectable
/// core of [`run`]. Nothing here reads `/proc/mounts` (or any env var), so a
/// test can hand it a real temp directory described as a `tmpfs` mount and
/// exercise scan → plan → remove end to end without a process-global seam
/// that races sibling tests. Production callers go through [`run`].
#[must_use]
pub fn run_with_mounts(
    mount_points: &[MountEntry],
    staleness_secs: u64,
    name_patterns: &[String],
    dry_run: bool,
    now: DateTime<Utc>,
) -> TmpfsReclaimReport {
    let candidates = scan_candidates(mount_points, name_patterns);
    let plan = plan_reclaim(&candidates, now, staleness_secs);
    let removed = if dry_run {
        Vec::new()
    } else {
        plan.remove
            .iter()
            .filter(|c| remove_candidate(c))
            .cloned()
            .collect()
    };
    TmpfsReclaimReport {
        enabled: true,
        dry_run,
        staleness_secs,
        plan: Some(plan),
        removed,
        deferred: None,
        at: now,
    }
}

/// Run one pass against every RAM-backed mount the host reports — the
/// CLI-facing entry point (`loom-daemon tmpfs-scratch-gc`), which bypasses
/// config/cooldown entirely (mirrors [`crate::target_dir_gc::run`]'s
/// standalone shape). A host with no readable mount table yields an empty
/// mount list, i.e. a clean no-op.
#[must_use]
pub fn run(
    staleness_secs: u64,
    name_patterns: &[String],
    dry_run: bool,
    now: DateTime<Utc>,
) -> TmpfsReclaimReport {
    run_with_mounts(&ram_backed_mount_points(), staleness_secs, name_patterns, dry_run, now)
}

/// Log one pass's outcome — `WARN` when it removed anything (an operator
/// should see multi-GB reclaims at default verbosity), `DEBUG` otherwise.
pub fn log_report(report: &TmpfsReclaimReport) {
    if !report.removed.is_empty() {
        log::warn!("tmpfs_reclaim: {}", report.reason());
    } else {
        log::debug!("tmpfs_reclaim: {}", report.reason());
    }
}

// ============================================================================
// Host-wide cooldown state (mirrors `docker_image_clean`)
// ============================================================================

/// Process-global "when did a pass last actually evaluate the mount table" —
/// host-wide (not per-repo): a `tmpfs` mount is not scoped to any one
/// registered repo, and a host with several repos ticking on the same reaper
/// cadence must not re-walk `/proc/mounts` once per repo per tick.
static LAST_EVALUATED_AT: OnceLock<Mutex<Option<DateTime<Utc>>>> = OnceLock::new();

fn last_evaluated_slot() -> &'static Mutex<Option<DateTime<Utc>>> {
    LAST_EVALUATED_AT.get_or_init(|| Mutex::new(None))
}

#[must_use]
fn cooldown_elapsed(now: DateTime<Utc>, min_interval_secs: u64) -> bool {
    let guard = last_evaluated_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match *guard {
        None => true,
        Some(last) => {
            let since = (now - last).num_seconds();
            since < 0 || since >= i64::try_from(min_interval_secs).unwrap_or(i64::MAX)
        }
    }
}

fn record_evaluated(now: DateTime<Utc>) {
    let mut guard = last_evaluated_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some(now);
}

/// Drop cooldown state. Test-only seam (the process-global would otherwise
/// leak between `#[serial]` tests in the same binary).
#[doc(hidden)]
pub fn reset_state_for_test() {
    *last_evaluated_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// Run one production pass, honoring the host-wide cooldown, and log the
/// result. Called once per registered repo per reaper tick (see
/// `worktree_reaper::reap_repo`) — every call after the first inside the
/// cooldown window is a cheap no-op (a clock read, no `/proc/mounts` read).
#[must_use]
pub fn run_for(repo_root: &Path) -> TmpfsReclaimReport {
    let config = read_tmpfs_reclaim_config(repo_root);
    let enabled = resolve_enabled(&config);
    let now = Utc::now();
    let staleness_secs = resolve_staleness_secs(&config);

    if !enabled {
        return TmpfsReclaimReport {
            enabled: false,
            dry_run: false,
            staleness_secs,
            plan: None,
            removed: Vec::new(),
            deferred: None,
            at: now,
        };
    }

    let min_interval_secs = resolve_min_interval_secs(&config);
    if !cooldown_elapsed(now, min_interval_secs) {
        return TmpfsReclaimReport {
            enabled,
            dry_run: false,
            staleness_secs,
            plan: None,
            removed: Vec::new(),
            deferred: Some(format!(
                "host-wide cooldown active (min interval {min_interval_secs}s) — \
                 the mount table was not read"
            )),
            at: now,
        };
    }

    let name_patterns = resolve_name_patterns(&config);
    let report = run(staleness_secs, &name_patterns, false, now);
    record_evaluated(now);
    log_report(&report);
    report
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serial_test::serial;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
    }

    fn set_mtime(path: &Path, epoch_offset_secs: i64) {
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let secs = (1_800_000_000 + epoch_offset_secs) as libc::time_t;
        let tv = libc::timeval {
            tv_sec: secs,
            tv_usec: 0,
        };
        let times = [tv, tv];
        let rc = unsafe { libc::utimes(c_path.as_ptr(), times.as_ptr()) };
        assert_eq!(rc, 0, "utimes failed for {}", path.display());
    }

    fn touch_with_mtime(path: &Path, secs: i64) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"fixture-payload").unwrap();
        set_mtime(path, secs);
    }

    fn candidate(
        name: &str,
        age_secs: i64,
        size_bytes: u64,
        has_open_handle: bool,
    ) -> ScratchDirCandidate {
        ScratchDirCandidate {
            path: PathBuf::from(name),
            mount_point: PathBuf::from("/dev/shm"),
            size_bytes,
            newest_mtime: t(-age_secs),
            has_open_handle,
        }
    }

    fn patterns() -> Vec<String> {
        DEFAULT_NAME_PATTERNS
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    /// Describe a real temp directory to the pass as though `/proc/mounts`
    /// had reported it as a `tmpfs` mount — the injection point that lets the
    /// end-to-end tests exercise scan → plan → remove without touching a real
    /// RAM-backed mount (or a process-global env seam).
    fn fake_tmpfs_mount(root: &Path) -> Vec<MountEntry> {
        vec![MountEntry {
            mount_point: root.to_path_buf(),
            fs_type: "tmpfs".to_string(),
        }]
    }

    // ===================================================================
    // /proc/mounts parsing + RAM-backed classification (pure, fixture-driven)
    // ===================================================================

    #[test]
    fn parses_a_realistic_proc_mounts_fixture() {
        let fixture = "\
udev /dev devtmpfs rw,nosuid,relatime 0 0
tmpfs /dev/shm tmpfs rw,nosuid,nodev 0 0
/dev/sda1 / ext4 rw,relatime 0 0
tmpfs /run/user/1000 tmpfs rw,nosuid,nodev,relatime 0 0
";
        let mounts = parse_mounts(fixture);
        assert_eq!(mounts.len(), 4);
        assert_eq!(mounts[1].mount_point, PathBuf::from("/dev/shm"));
        assert_eq!(mounts[1].fs_type, "tmpfs");
    }

    #[test]
    fn ram_backed_filter_keeps_only_tmpfs_and_ramfs() {
        let fixture = "\
udev /dev devtmpfs rw 0 0
tmpfs /dev/shm tmpfs rw 0 0
/dev/sda1 / ext4 rw 0 0
none /mnt/ram ramfs rw 0 0
overlay /var/lib/docker/overlay2/abc overlay rw 0 0
";
        let mounts = ram_backed_mount_points_from(fixture);
        let points: Vec<&Path> = mounts.iter().map(|m| m.mount_point.as_path()).collect();
        assert_eq!(points, vec![Path::new("/dev/shm"), Path::new("/mnt/ram")]);
    }

    #[test]
    fn unescapes_octal_whitespace_in_mount_points() {
        // A mount point containing a space is escaped as \040 in /proc/mounts.
        let fixture = "tmpfs /mnt/my\\040shm tmpfs rw 0 0\n";
        let mounts = parse_mounts(fixture);
        assert_eq!(mounts[0].mount_point, PathBuf::from("/mnt/my shm"));
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        let fixture = "garbage-one-field\ntmpfs /dev/shm tmpfs rw 0 0\n\nbad two\n";
        let mounts = parse_mounts(fixture);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].mount_point, PathBuf::from("/dev/shm"));
    }

    // `MOUNTS_FILE_ENV` and `TMPFS_RECLAIM_ENABLE_ENV` are process-global, so
    // every test that touches one shares the `tmpfs_reclaim_env` serial group
    // — otherwise a sibling test's `remove_var` lands between another's
    // `set_var` and its read, and the second test silently falls through to
    // the host's real `/proc/mounts`.
    #[test]
    #[serial(tmpfs_reclaim_env)]
    fn ram_backed_mount_points_degrades_to_empty_on_unreadable_file() {
        let missing = "/nonexistent/path/for/loom-tmpfs-reclaim-test/proc-mounts";
        std::env::set_var(MOUNTS_FILE_ENV, missing);
        let mounts = ram_backed_mount_points();
        std::env::remove_var(MOUNTS_FILE_ENV);
        assert!(
            mounts.is_empty(),
            "an unreadable mounts file must be a no-op, not a panic/error"
        );
    }

    #[test]
    #[serial(tmpfs_reclaim_env)]
    fn ram_backed_mount_points_reads_the_mounts_file_seam() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = tmp.path().join("proc-mounts");
        fs::write(&fixture, "/dev/sda1 / ext4 rw 0 0\ntmpfs /dev/shm tmpfs rw 0 0\n").unwrap();

        std::env::set_var(MOUNTS_FILE_ENV, &fixture);
        let mounts = ram_backed_mount_points();
        std::env::remove_var(MOUNTS_FILE_ENV);

        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].mount_point, PathBuf::from("/dev/shm"));
        assert_eq!(mounts[0].fs_type, "tmpfs");
    }

    // ===================================================================
    // Name-pattern matching (pure)
    // ===================================================================

    #[test]
    fn prefix_wildcard_matches_expected_names() {
        assert!(matches_pattern("cargo-target-42", "cargo-target-*"));
        assert!(matches_pattern("cargo-target-", "cargo-target-*"));
        assert!(!matches_pattern("cargo-target", "cargo-target-*"));
        assert!(!matches_pattern("other-dir", "cargo-target-*"));
    }

    #[test]
    fn suffix_wildcard_matches_expected_names() {
        assert!(matches_pattern("tmp-issue42", "tmp-issue*"));
        assert!(matches_pattern("tmp-issue", "tmp-issue*"));
        assert!(!matches_pattern("issue-tmp-42", "tmp-issue*"));
    }

    #[test]
    fn exact_pattern_with_no_wildcard_requires_exact_match() {
        assert!(matches_pattern("exact-name", "exact-name"));
        assert!(!matches_pattern("exact-name-2", "exact-name"));
    }

    #[test]
    fn matches_name_patterns_checks_every_pattern_in_the_set() {
        let pats = patterns();
        assert!(matches_name_patterns("cargo-target-99", &pats));
        assert!(matches_name_patterns("tmp-issue7", &pats));
        assert!(!matches_name_patterns("unrelated-dir", &pats));
    }

    // ===================================================================
    // ScratchDirCandidate::is_eligible / plan_reclaim — the pure core
    // ===================================================================

    #[test]
    fn a_stale_orphan_with_no_open_handle_is_eligible() {
        let c = candidate("a", (DEFAULT_STALENESS_SECS as i64) + 1, 100, false);
        assert!(c.is_eligible(t(0), DEFAULT_STALENESS_SECS));
    }

    #[test]
    fn a_dir_with_an_open_handle_is_never_eligible_even_if_old() {
        let c = candidate("a", (DEFAULT_STALENESS_SECS as i64) * 10, 100, true);
        assert!(!c.is_eligible(t(0), DEFAULT_STALENESS_SECS));
    }

    #[test]
    fn a_dir_younger_than_the_staleness_window_is_kept() {
        let c = candidate("a", 60, 100, false);
        assert!(!c.is_eligible(t(0), DEFAULT_STALENESS_SECS));
    }

    #[test]
    fn exactly_at_the_staleness_window_is_eligible() {
        let c = candidate("a", DEFAULT_STALENESS_SECS as i64, 100, false);
        assert!(c.is_eligible(t(0), DEFAULT_STALENESS_SECS));
    }

    #[test]
    fn plan_reclaim_separates_eligible_from_kept_and_sums_bytes() {
        let candidates = vec![
            candidate("orphan", (DEFAULT_STALENESS_SECS as i64) + 100, 500, false),
            candidate("fresh", 60, 200, false),
            candidate("in-use", (DEFAULT_STALENESS_SECS as i64) + 100, 900, true),
        ];
        let plan = plan_reclaim(&candidates, t(0), DEFAULT_STALENESS_SECS);
        assert_eq!(plan.remove.len(), 1);
        assert_eq!(plan.remove[0].path, PathBuf::from("orphan"));
        assert_eq!(plan.keep.len(), 2);
        assert_eq!(plan.bytes_to_remove(), 500);
    }

    #[test]
    fn empty_plan_summarizes_as_nothing() {
        assert_eq!(ReclaimPlan::default().summary(), "nothing");
    }

    #[test]
    fn plan_summary_reports_count_and_size() {
        let candidates = vec![candidate(
            "orphan",
            (DEFAULT_STALENESS_SECS as i64) + 1,
            2 * 1024 * 1024 * 1024,
            false,
        )];
        let plan = plan_reclaim(&candidates, t(0), DEFAULT_STALENESS_SECS);
        assert!(plan.summary().contains("1 scratch dir"));
        assert!(plan.summary().contains("2.0G"));
    }

    // ===================================================================
    // scan_candidates — I/O integration against a real (non-tmpfs) tempdir
    // standing in for a classified mount, proving name-pattern + mount-scope
    // discipline end to end.
    // ===================================================================

    #[test]
    fn scan_only_picks_up_loom_named_dirs_directly_under_a_mount() {
        let tmp = tempfile::tempdir().unwrap();
        let mount_root = tmp.path();

        touch_with_mtime(&mount_root.join("cargo-target-42/debug/x.bin"), -1);
        touch_with_mtime(&mount_root.join("tmp-issue7/scratch.txt"), -1);
        touch_with_mtime(&mount_root.join("unrelated-app-cache/y.bin"), -1);

        let mounts = vec![MountEntry {
            mount_point: mount_root.to_path_buf(),
            fs_type: "tmpfs".to_string(),
        }];
        let candidates = scan_candidates(&mounts, &patterns());

        let names: Vec<String> = candidates
            .iter()
            .map(|c| c.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(candidates.len(), 2, "{names:?}");
        assert!(names.contains(&"cargo-target-42".to_string()));
        assert!(names.contains(&"tmp-issue7".to_string()));
        assert!(!names.contains(&"unrelated-app-cache".to_string()));
    }

    #[test]
    fn scan_never_looks_at_a_mount_not_in_the_ram_backed_list() {
        // A directory tree that would match every naming rule, but whose
        // mount point is simply never passed in (i.e. it was excluded by
        // `ram_backed_mount_points` for not being tmpfs/ramfs) — proving
        // "non-tmpfs is never touched regardless of name" by construction:
        // there is no code path from a non-RAM-backed mount into a candidate.
        let tmp = tempfile::tempdir().unwrap();
        touch_with_mtime(&tmp.path().join("cargo-target-should-never-be-seen/x.bin"), -1);

        let candidates = scan_candidates(&[], &patterns());
        assert!(candidates.is_empty());
    }

    #[test]
    fn scan_ignores_a_symlink_wearing_a_loom_scratch_name() {
        // The worst case a name-pattern-driven reclaim could hit: a symlink
        // planted on the tmpfs mount, named to match, pointing somewhere
        // valuable. `DirEntry::file_type` does not traverse symlinks, so the
        // link is not a directory and never becomes a candidate — meaning
        // `remove_dir_all` can never be aimed through it at the target.
        let tmp = tempfile::tempdir().unwrap();
        let mount_root = tmp.path();
        let precious = mount_root.join("precious");
        touch_with_mtime(&precious.join("do-not-delete.txt"), -100_000);
        std::os::unix::fs::symlink(&precious, mount_root.join("cargo-target-evil")).unwrap();

        let candidates = scan_candidates(&fake_tmpfs_mount(mount_root), &patterns());
        assert!(candidates.is_empty(), "a symlink must never be a candidate");

        let report = run_with_mounts(
            &fake_tmpfs_mount(mount_root),
            DEFAULT_STALENESS_SECS,
            &patterns(),
            false,
            t(0),
        );
        assert!(report.removed.is_empty());
        assert!(precious.join("do-not-delete.txt").exists());
    }

    #[test]
    fn scan_never_descends_below_the_mount_root_for_candidates() {
        // Only DIRECT children of a classified mount are candidates: a
        // matching name nested one level down must not be reached, so the
        // pass can never wander into an unrelated subtree that happens to
        // contain a Loom-shaped name.
        let tmp = tempfile::tempdir().unwrap();
        let mount_root = tmp.path();
        touch_with_mtime(
            &mount_root.join("someones-app/cargo-target-99/x.bin"),
            -((DEFAULT_STALENESS_SECS as i64) + 100),
        );

        let candidates = scan_candidates(&fake_tmpfs_mount(mount_root), &patterns());
        assert!(candidates.is_empty());
        assert!(mount_root.join("someones-app/cargo-target-99").exists());
    }

    #[test]
    fn scan_reports_recursive_size_and_newest_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let mount_root = tmp.path();
        touch_with_mtime(&mount_root.join("cargo-target-1/a.bin"), -100);
        touch_with_mtime(&mount_root.join("cargo-target-1/nested/b.bin"), -1);

        let mounts = vec![MountEntry {
            mount_point: mount_root.to_path_buf(),
            fs_type: "tmpfs".to_string(),
        }];
        let candidates = scan_candidates(&mounts, &patterns());
        assert_eq!(candidates.len(), 1);
        let c = &candidates[0];
        assert_eq!(c.size_bytes, 2 * b"fixture-payload".len() as u64);
        // Newest mtime must reflect the more-recently-written nested file
        // (-1s), not the older top-level one (-100s).
        assert!(c.newest_mtime > t(-50));
    }

    // ===================================================================
    // run() end-to-end — dry-run vs real
    // ===================================================================

    #[test]
    fn dry_run_reports_the_plan_but_removes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mount_root = tmp.path();
        touch_with_mtime(
            &mount_root.join("cargo-target-old/x.bin"),
            -((DEFAULT_STALENESS_SECS as i64) + 100),
        );

        let report = run_with_mounts(
            &fake_tmpfs_mount(mount_root),
            DEFAULT_STALENESS_SECS,
            &patterns(),
            true,
            t(0),
        );

        assert!(report.dry_run);
        assert_eq!(report.plan.as_ref().unwrap().remove.len(), 1);
        assert!(report.removed.is_empty());
        assert_eq!(report.bytes_reclaimed(), 0);
        assert!(mount_root.join("cargo-target-old").exists(), "dry run must not delete anything");
    }

    #[test]
    fn a_real_run_removes_only_eligible_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let mount_root = tmp.path();
        touch_with_mtime(
            &mount_root.join("cargo-target-old/x.bin"),
            -((DEFAULT_STALENESS_SECS as i64) + 100),
        );
        touch_with_mtime(&mount_root.join("cargo-target-new/x.bin"), -60);
        touch_with_mtime(
            &mount_root.join("someone-elses-cache/x.bin"),
            -((DEFAULT_STALENESS_SECS as i64) + 100),
        );

        let report = run_with_mounts(
            &fake_tmpfs_mount(mount_root),
            DEFAULT_STALENESS_SECS,
            &patterns(),
            false,
            t(0),
        );

        assert_eq!(report.removed.len(), 1);
        assert!(!mount_root.join("cargo-target-old").exists(), "the stale orphan must be gone");
        assert!(mount_root.join("cargo-target-new").exists(), "the fresh dir must survive");
        assert!(
            mount_root.join("someone-elses-cache").exists(),
            "an unrecognized name must survive however stale it is"
        );
    }

    #[test]
    fn a_run_over_no_ram_backed_mounts_is_a_clean_no_op() {
        let report = run_with_mounts(&[], DEFAULT_STALENESS_SECS, &patterns(), false, t(0));
        assert!(report.enabled);
        assert_eq!(report.plan.as_ref().unwrap().remove.len(), 0);
        assert!(report.removed.is_empty());
        assert_eq!(report.reason(), "reclaimed 0B of planned 0B (0 kept)");
    }

    // ===================================================================
    // Config resolution defaults
    // ===================================================================

    #[test]
    #[serial(tmpfs_reclaim_env)]
    fn default_config_resolves_to_documented_defaults() {
        let config = TmpfsReclaimConfig::default();
        assert!(resolve_enabled(&config));
        assert_eq!(resolve_staleness_secs(&config), DEFAULT_STALENESS_SECS);
        assert_eq!(resolve_min_interval_secs(&config), DEFAULT_MIN_INTERVAL_SECS);
        assert_eq!(resolve_name_patterns(&config), patterns());
    }

    #[test]
    #[serial(tmpfs_reclaim_env)]
    fn config_values_override_defaults() {
        let config = TmpfsReclaimConfig {
            enabled: Some(false),
            staleness_secs: Some(3_600),
            min_interval_secs: Some(300),
            name_patterns: Some(vec!["my-scratch-*".to_string()]),
        };
        assert!(!resolve_enabled(&config));
        assert_eq!(resolve_staleness_secs(&config), 3_600);
        assert_eq!(resolve_min_interval_secs(&config), 300);
        assert_eq!(resolve_name_patterns(&config), vec!["my-scratch-*".to_string()]);
    }

    #[test]
    #[serial(tmpfs_reclaim_env)]
    fn env_overrides_both_config_and_default() {
        let config = TmpfsReclaimConfig {
            enabled: Some(true),
            staleness_secs: Some(3_600),
            min_interval_secs: Some(300),
            name_patterns: None,
        };
        std::env::set_var(TMPFS_RECLAIM_ENABLE_ENV, "0");
        std::env::set_var(TMPFS_RECLAIM_STALENESS_ENV, "90");
        std::env::set_var(TMPFS_RECLAIM_MIN_INTERVAL_ENV, "45");
        let (enabled, staleness, interval) = (
            resolve_enabled(&config),
            resolve_staleness_secs(&config),
            resolve_min_interval_secs(&config),
        );
        std::env::remove_var(TMPFS_RECLAIM_ENABLE_ENV);
        std::env::remove_var(TMPFS_RECLAIM_STALENESS_ENV);
        std::env::remove_var(TMPFS_RECLAIM_MIN_INTERVAL_ENV);

        assert!(!enabled, "env must be able to disable a config-enabled pass");
        assert_eq!(staleness, 90);
        assert_eq!(interval, 45);
    }

    #[test]
    #[serial(tmpfs_reclaim_env)]
    fn read_config_reads_the_tmpfs_scratch_gc_block() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".loom")).unwrap();
        fs::write(
            tmp.path().join(".loom/config.json"),
            r#"{"autonomous":{"tmpfsScratchGc":{
                "enabled": false,
                "stalenessSecs": 7200,
                "minIntervalSecs": 600,
                "namePatterns": ["scratch-*"]
            }}}"#,
        )
        .unwrap();

        let config = read_tmpfs_reclaim_config(tmp.path());
        assert_eq!(config.enabled, Some(false));
        assert_eq!(config.staleness_secs, Some(7_200));
        assert_eq!(config.min_interval_secs, Some(600));
        assert_eq!(config.name_patterns, Some(vec!["scratch-*".to_string()]));
    }

    #[test]
    #[serial(tmpfs_reclaim_env)]
    fn read_config_soft_fails_to_defaults_on_a_repo_with_no_block() {
        let tmp = tempfile::tempdir().unwrap();
        let config = read_tmpfs_reclaim_config(tmp.path());
        assert_eq!(config, TmpfsReclaimConfig::default());
    }

    #[test]
    #[serial(tmpfs_reclaim_env)]
    fn read_config_soft_fails_to_defaults_on_malformed_json() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".loom")).unwrap();
        fs::write(tmp.path().join(".loom/config.json"), "{ not json").unwrap();
        assert_eq!(read_tmpfs_reclaim_config(tmp.path()), TmpfsReclaimConfig::default());
    }

    // ===================================================================
    // Cooldown (host-wide, not per-repo)
    // ===================================================================

    #[test]
    #[serial(tmpfs_reclaim_env)]
    fn cooldown_elapsed_is_true_before_any_evaluation() {
        reset_state_for_test();
        assert!(cooldown_elapsed(t(0), 1_800));
        reset_state_for_test();
    }

    #[test]
    #[serial(tmpfs_reclaim_env)]
    fn a_recent_evaluation_holds_the_cooldown() {
        reset_state_for_test();
        record_evaluated(t(0));
        assert!(!cooldown_elapsed(t(60), 1_800));
        reset_state_for_test();
    }

    #[test]
    #[serial(tmpfs_reclaim_env)]
    fn the_cooldown_expires() {
        reset_state_for_test();
        record_evaluated(t(0));
        assert!(cooldown_elapsed(t(1_801), 1_800));
        reset_state_for_test();
    }

    #[test]
    #[serial(tmpfs_reclaim_env)]
    fn run_for_disabled_never_reads_the_mount_table() {
        reset_state_for_test();
        std::env::set_var(TMPFS_RECLAIM_ENABLE_ENV, "0");
        // Point at a fixture that would panic-worthy-assert if ever read by
        // making it a directory (unreadable as a mounts file) — the disabled
        // short-circuit must never even attempt to read it.
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var(MOUNTS_FILE_ENV, tmp.path());
        let report = run_for(tmp.path());
        std::env::remove_var(TMPFS_RECLAIM_ENABLE_ENV);
        std::env::remove_var(MOUNTS_FILE_ENV);
        reset_state_for_test();

        assert!(!report.enabled);
        assert!(report.plan.is_none());
        assert!(report.removed.is_empty());
    }

    #[test]
    #[serial(tmpfs_reclaim_env)]
    fn run_for_defers_the_second_call_inside_the_host_wide_cooldown() {
        reset_state_for_test();
        let tmp = tempfile::tempdir().unwrap();
        // An empty fixture mount table: the pass has nothing to scan, so this
        // exercises the cooldown bookkeeping without touching the real host.
        let fixture = tmp.path().join("proc-mounts");
        fs::write(&fixture, "/dev/sda1 / ext4 rw 0 0\n").unwrap();
        std::env::set_var(MOUNTS_FILE_ENV, &fixture);

        let first = run_for(tmp.path());
        let second = run_for(tmp.path());

        std::env::remove_var(MOUNTS_FILE_ENV);
        reset_state_for_test();

        assert!(first.enabled);
        assert!(
            first.plan.is_some() && first.deferred.is_none(),
            "the first call must actually evaluate"
        );
        assert!(
            second.plan.is_none() && second.deferred.is_some(),
            "a second call inside the cooldown must defer, not re-walk the mount table"
        );
        assert!(second.reason().starts_with("deferred: "));
    }
}
