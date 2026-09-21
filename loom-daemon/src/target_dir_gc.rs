//! Shared cargo `target/` directory GC — prune stale `incremental/` session
//! artifacts and `deps/` files out of a long-lived, cross-worktree cargo
//! target directory (issue #8459, decomposed from #8453 item 3).
//!
//! # What was leaking
//!
//! Nothing prunes a shared cargo target directory (the kind produced by
//! `CARGO_TARGET_DIR` or `build.target-dir` redirection — see
//! [`crate::worktree_ops::cargo_target`] and #7239 for the per-worktree
//! *removal* side of that convention). Measured on one host, 2026-09-20:
//! `debug/incremental/` at 213 GB (6,402 session dirs — one per worktree path
//! ever built) and `debug/deps/` at 231 GB, with roughly half of
//! `incremental/` and ~80% of `deps/`-adjacent content untouched for 3–7+
//! days. Cargo treats both directories as pure caches — any unit whose output
//! is missing is simply rebuilt — so pruning is a rebuild-cost decision only,
//! never a durability one.
//!
//! # Shape: a manual/cron subcommand, not a periodic-reaper pass
//!
//! Unlike [`crate::deep_clean`] and [`crate::docker_image_clean`] — which ride
//! the registered-repo [`crate::worktree_reaper`] tick because their targets
//! (a repo's own primary-checkout `target/`, or the host's Docker image
//! store) are each keyed off one thing the reaper already iterates —  a
//! *shared* cargo target dir has no such natural home: it is host-configured
//! (`CARGO_TARGET_DIR` / `~/.cargo/config.toml`'s `build.target-dir`), can be
//! shared across every registered repo on the host, and is not "the primary
//! checkout of repo X" for any single X the reaper walks. Wiring it into that
//! per-repo cadence would mean re-deriving (and de-duplicating) the shared
//! path once per registered repo per tick for no benefit over a single
//! path-scoped invocation. This module is therefore invoked explicitly — the
//! `loom-daemon target-dir-gc` subcommand — on a manual or cron cadence an
//! operator controls; see `defaults/docs/troubleshooting.md` → "Shared cargo
//! target-dir GC" for the recommended cron line. A future issue can wire this
//! into a cadence once a host-level (not per-repo) periodic hook exists for
//! it to ride.
//!
//! # Two-layer safety, so a running build is never touched
//!
//! 1. **The global cargo build lock.** Every `cargo` invocation takes an
//!    advisory lock on `<target_dir>/.cargo-lock` for the duration of the
//!    build. [`cargo_lock_is_held`] attempts a non-blocking exclusive `flock`
//!    on that file — if *any* concurrent cargo process holds even a shared
//!    lock, the attempt fails and this pass **defers entirely**: nothing is
//!    scanned for removal, dry-run or not. This is deliberately coarse (a
//!    build anywhere under this target dir defers the whole pass, not just
//!    the crate it is building) because it is the one signal that is true
//!    regardless of cargo version or incremental-cache internals.
//! 2. **Recursive mtime freshness, not directory-mtime-alone.** A directory's
//!    own mtime only changes when an entry is added or removed from it — not
//!    when an existing file inside it is overwritten. Judging staleness by a
//!    session directory's own mtime would therefore misjudge a directory
//!    whose *files* are being actively rewritten (the incremental-compile
//!    case) as stale the moment it stops receiving *new* files. [`scan`]
//!    computes the **newest mtime found anywhere under each candidate**
//!    (itself, for a plain `deps/` file) and that is what staleness is judged
//!    against — "age alone" (the directory's own mtime) is never the input.
//!
//! Both gates are conservative in the same direction: an inability to prove a
//! candidate is safe to remove keeps it, never the reverse.
//!
//! # Reporting
//!
//! [`run`] always computes and returns the plan (which candidates *would* be
//! removed and their total bytes) whether or not `dry_run` is set — a dry run
//! is exactly a real run with the removal step skipped, not a separate code
//! path, so the two can never drift apart. [`GcReport::bytes_reclaimed`] is 0
//! on a dry run or a deferred (build-in-progress) pass; the sanity-check value
//! an operator wants before a first real run is [`GcReport::bytes_reclaimable`].

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

// ============================================================================
// Constants
// ============================================================================

/// Default staleness threshold: 7 days.
pub const DEFAULT_THRESHOLD_DAYS: u64 = 7;

/// Directory search is bounded to this many levels below `target_dir` when
/// looking for `incremental/`/`deps/` directories — cargo's own layout never
/// nests them deeper than `<target_dir>/<profile>/<optional target-triple>/`,
/// so this is generous headroom, not a real limit in practice.
const MAX_SEARCH_DEPTH: u32 = 6;

// ============================================================================
// Candidate model
// ============================================================================

/// Which cargo cache class a [`PruneCandidate`] belongs to — carried through
/// purely for reporting (`"3 incremental sessions (1.2G), 40 deps files
/// (600M)"`); both are pruned under the identical age rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// One `s-*` entry (file or directory) inside
    /// `incremental/<crate-fingerprint>/`.
    IncrementalSession,
    /// One file directly inside a `deps/` directory.
    DepsFile,
}

impl EntryKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::IncrementalSession => "incremental session",
            Self::DepsFile => "deps file",
        }
    }
}

/// One removable (or kept) unit under the scanned target directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneCandidate {
    pub path: PathBuf,
    pub kind: EntryKind,
    /// The newest mtime found anywhere under `path` (itself, for a plain
    /// file) — see the module docs' "recursive mtime freshness" gate.
    pub newest_mtime: DateTime<Utc>,
    /// Total on-disk size in bytes (recursive for a directory).
    pub size_bytes: u64,
}

impl PruneCandidate {
    /// Whether `self` is at least `threshold_days` old as of `now`, judged
    /// against [`Self::newest_mtime`] — never a directory's own mtime. A
    /// `threshold_days` of `0` means "no age floor at all": every candidate
    /// is stale unless its newest mtime is literally in the future relative
    /// to `now` (impossible in practice bar clock skew). This is intentional
    /// — see the module docs and [`run`]'s `--threshold-days 0` behavior —
    /// not an accidental edge case, and it is why `0` should be reserved for
    /// an operator who deliberately wants a full unconditional prune (a
    /// negative age clamps to `0`, which is still `>= 0`).
    #[must_use]
    pub fn is_stale(&self, now: DateTime<Utc>, threshold_days: u64) -> bool {
        let age_secs = (now - self.newest_mtime).num_seconds().max(0);
        let threshold_secs =
            i64::try_from(threshold_days.saturating_mul(86_400)).unwrap_or(i64::MAX);
        age_secs >= threshold_secs
    }
}

fn human_size(bytes: u64) -> String {
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

// ============================================================================
// Pure planning
// ============================================================================

/// The disposition of one scan — every candidate ends up in exactly one
/// bucket. Pure function of `(candidates, now, threshold_days)`: no I/O, fully
/// unit-testable against a fixture list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrunePlan {
    pub remove: Vec<PruneCandidate>,
    pub keep: Vec<PruneCandidate>,
}

impl PrunePlan {
    #[must_use]
    pub fn bytes_to_remove(&self) -> u64 {
        self.remove.iter().map(|c| c.size_bytes).sum()
    }

    /// `"3 incremental session (1.2G), 2 deps file (600.0M)"`, or `"nothing"`.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.remove.is_empty() {
            return "nothing".to_string();
        }
        let mut counts: Vec<(EntryKind, usize, u64)> = Vec::new();
        for kind in [EntryKind::IncrementalSession, EntryKind::DepsFile] {
            let matching: Vec<&PruneCandidate> =
                self.remove.iter().filter(|c| c.kind == kind).collect();
            if !matching.is_empty() {
                let bytes: u64 = matching.iter().map(|c| c.size_bytes).sum();
                counts.push((kind, matching.len(), bytes));
            }
        }
        counts
            .into_iter()
            .map(|(kind, n, bytes)| {
                let plural = if n == 1 { "" } else { "s" };
                format!("{n} {}{plural} ({})", kind.label(), human_size(bytes))
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Decide what to remove, purely from an already-scanned candidate list. Every
/// candidate has already had its "is a build using this?" safety check
/// resolved by [`scan`] into `newest_mtime` (a locked/actively-written entry
/// reads as freshly-mtimed) — this function only ever applies the age rule.
#[must_use]
pub fn plan_prune(
    candidates: &[PruneCandidate],
    now: DateTime<Utc>,
    threshold_days: u64,
) -> PrunePlan {
    let mut plan = PrunePlan::default();
    for candidate in candidates {
        if candidate.is_stale(now, threshold_days) {
            plan.remove.push(candidate.clone());
        } else {
            plan.keep.push(candidate.clone());
        }
    }
    plan
}

// ============================================================================
// I/O: the global build-lock check
// ============================================================================

/// Whether a cargo build currently holds (or might hold — see below) the
/// advisory lock cargo takes on `<target_dir>/.cargo-lock` for the duration
/// of every build. `Ok(true)` blocks the entire pass in [`run`].
///
/// - The lock file not existing at all is `Ok(false)` — no build has ever run
///   against this target dir, which is not evidence of one running now.
/// - Any other I/O failure trying to open or lock the file (permissions, a
///   transient FS error, …) is treated as `Ok(true)` — **unmeasurable is
///   never treated as unlocked** in a safety check whose only job is "did a
///   build touch this recently", the same discipline
///   [`crate::deep_clean::DeepCleanTrigger::FreeSpaceUnknown`] applies to an
///   unmeasurable `df`.
#[must_use]
pub fn cargo_lock_is_held(target_dir: &Path) -> bool {
    let lock_path = target_dir.join(".cargo-lock");
    let file = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    match try_lock_exclusive(&file) {
        Ok(true) => {
            // We took the lock ourselves — release it immediately and report
            // "not held".
            let _ = unlock(&file);
            false
        }
        Ok(false) => true,
        Err(_) => true,
    }
}

#[cfg(unix)]
fn try_lock_exclusive(file: &fs::File) -> io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is a valid, open file descriptor for the lifetime of this
    // call (borrowed from `file`, which outlives it).
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(true)
    } else {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            Ok(false)
        } else {
            Err(err)
        }
    }
}

#[cfg(unix)]
fn unlock(file: &fs::File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: see `try_lock_exclusive`.
    let rc = unsafe { libc::flock(fd, libc::LOCK_UN) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn try_lock_exclusive(_file: &fs::File) -> io::Result<bool> {
    // No portable non-blocking advisory lock on this platform; treat the file
    // as unlocked (relies entirely on the mtime-freshness gate). Not exercised
    // by the daemon's supported (Unix) hosts today.
    Ok(true)
}

#[cfg(not(unix))]
fn unlock(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

// ============================================================================
// I/O: scanning
// ============================================================================

/// The result of walking `target_dir` for prunable candidates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetDirScan {
    pub candidates: Vec<PruneCandidate>,
}

/// Recursively find every newest-mtime and cumulative size under `path`. For
/// a plain file this is just that file's own metadata; for a directory it is
/// the max mtime and summed size of everything inside — the mechanism behind
/// the module docs' "recursive mtime freshness, not directory-mtime-alone"
/// safety gate. Best-effort: an unreadable child is skipped rather than
/// failing the whole walk (a permissions hiccup on one nested file must not
/// abort GC of everything else).
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

/// Find every directory named exactly `want` under `root`, depth-bounded by
/// [`MAX_SEARCH_DEPTH`]. Cargo's own layout (`<profile>/incremental`,
/// `<profile>/<target-triple>/deps`, …) is always shallow; the bound exists
/// only to keep a pathological input (a symlink cycle, an unrelated huge
/// tree pointed at by mistake) from an unbounded walk.
fn find_dirs_named(root: &Path, want: &str, depth: u32) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if depth > MAX_SEARCH_DEPTH {
        return found;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        if entry.file_name() == want {
            found.push(path);
        } else {
            found.extend(find_dirs_named(&path, want, depth + 1));
        }
    }
    found
}

/// Scan `target_dir` for prunable `incremental/`/`deps/` entries.
///
/// A `target_dir` that does not exist yet scans as empty (`Ok` with no
/// candidates) — this is a normal state (nothing has ever been built there),
/// not an error. Every other I/O failure while walking is best-effort: a
/// single unreadable subtree is skipped, never fails the whole scan (matching
/// [`newest_mtime_and_size`]'s discipline).
pub fn scan(target_dir: &Path) -> io::Result<TargetDirScan> {
    if !target_dir.exists() {
        return Ok(TargetDirScan::default());
    }

    let mut candidates = Vec::new();

    for incremental_dir in find_dirs_named(target_dir, "incremental", 0) {
        let Ok(crate_dirs) = fs::read_dir(&incremental_dir) else {
            continue;
        };
        for crate_dir in crate_dirs.flatten() {
            let crate_path = crate_dir.path();
            if !crate_path.is_dir() {
                continue;
            }
            let Ok(sessions) = fs::read_dir(&crate_path) else {
                continue;
            };
            for session in sessions.flatten() {
                let session_path = session.path();
                // `.lock` files are cargo's own bookkeeping, never a
                // reclaimable cache artifact in their own right — they are
                // covered by the global `.cargo-lock` gate at the target-dir
                // root instead, and this repo's `.gitignore`-shaped skip list
                // has no bearing here (this walk never touches git).
                if session_path.extension().is_some_and(|ext| ext == "lock") {
                    continue;
                }
                if let Ok((newest_mtime, size_bytes)) = newest_mtime_and_size(&session_path) {
                    candidates.push(PruneCandidate {
                        path: session_path,
                        kind: EntryKind::IncrementalSession,
                        newest_mtime,
                        size_bytes,
                    });
                }
            }
        }
    }

    for deps_dir in find_dirs_named(target_dir, "deps", 0) {
        let Ok(files) = fs::read_dir(&deps_dir) else {
            continue;
        };
        for file in files.flatten() {
            let file_path = file.path();
            if let Ok((newest_mtime, size_bytes)) = newest_mtime_and_size(&file_path) {
                candidates.push(PruneCandidate {
                    path: file_path,
                    kind: EntryKind::DepsFile,
                    newest_mtime,
                    size_bytes,
                });
            }
        }
    }

    Ok(TargetDirScan { candidates })
}

// ============================================================================
// The pass
// ============================================================================

/// One GC pass's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcReport {
    pub target_dir: PathBuf,
    pub threshold_days: u64,
    pub dry_run: bool,
    /// `true` when the global cargo build-lock gate deferred the entire pass
    /// — [`plan`] is still populated (for observability, mirroring
    /// [`crate::deep_clean::DeepCleanReport`]) but [`removed`] is always empty
    /// in that case, dry-run or not.
    pub build_in_progress: bool,
    /// What the pass decided to (or would) remove.
    pub plan: PrunePlan,
    /// What was **actually** deleted this pass — empty when `dry_run` is set,
    /// when `build_in_progress` is true, or when every removal attempt
    /// failed.
    pub removed: Vec<PruneCandidate>,
    pub at: DateTime<Utc>,
}

impl GcReport {
    /// Bytes actually freed by this pass — 0 on a dry run or a deferred pass.
    #[must_use]
    pub fn bytes_reclaimed(&self) -> u64 {
        self.removed.iter().map(|c| c.size_bytes).sum()
    }

    /// Bytes the plan identifies as removable, whether or not this pass
    /// removed them — the sanity-check number a dry run reports.
    #[must_use]
    pub fn bytes_reclaimable(&self) -> u64 {
        self.plan.bytes_to_remove()
    }

    /// One-line rationale for logging / CLI output.
    #[must_use]
    pub fn reason(&self) -> String {
        if self.build_in_progress {
            return format!(
                "a cargo build appears to be using {} (the .cargo-lock advisory lock is held, \
                 or its state is unknown) — deferring the entire pass, nothing was touched",
                self.target_dir.display()
            );
        }
        if self.dry_run {
            return format!(
                "DRY RUN: would reclaim {} ({} candidates, {} kept)",
                human_size(self.bytes_reclaimable()),
                self.plan.remove.len(),
                self.plan.keep.len()
            );
        }
        format!(
            "reclaimed {} of planned {} ({} kept)",
            human_size(self.bytes_reclaimed()),
            human_size(self.bytes_reclaimable()),
            self.plan.keep.len()
        )
    }
}

/// Remove one candidate (a directory or a plain file). A failure is a soft,
/// per-candidate failure — logged and excluded from the report's `removed`
/// list, never fatal to the rest of the pass (mirrors
/// [`crate::docker_image_clean::remove_image`]'s per-image failure handling).
fn remove_candidate(candidate: &PruneCandidate) -> bool {
    let result = if candidate.path.is_dir() {
        fs::remove_dir_all(&candidate.path)
    } else {
        fs::remove_file(&candidate.path)
    };
    match result {
        Ok(()) => true,
        Err(e) => {
            log::warn!("target_dir_gc: could not remove {}: {e}", candidate.path.display());
            false
        }
    }
}

/// Run one GC pass against `target_dir`.
///
/// This is the single code path for both dry-run and real invocations: the
/// plan is always computed the same way, and `dry_run` only gates whether
/// [`remove_candidate`] is actually called — see the module docs.
#[must_use]
pub fn run(target_dir: &Path, threshold_days: u64, dry_run: bool, now: DateTime<Utc>) -> GcReport {
    let build_in_progress = cargo_lock_is_held(target_dir);
    let scanned = scan(target_dir).unwrap_or_default();
    // The plan is always computed, build-in-progress or not — purely for
    // observability in the deferred case (`GcReport::reason` reports what
    // WOULD have been considered). Only actual removal is gated below.
    let plan = plan_prune(&scanned.candidates, now, threshold_days);

    let removed = if build_in_progress || dry_run {
        Vec::new()
    } else {
        plan.remove
            .iter()
            .filter(|c| remove_candidate(c))
            .cloned()
            .collect()
    };

    GcReport {
        target_dir: target_dir.to_path_buf(),
        threshold_days,
        dry_run,
        build_in_progress,
        plan,
        removed,
        at: now,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::fs::File;
    use std::os::unix::ffi::OsStrExt;

    fn t(secs: i64) -> DateTime<Utc> {
        chrono::TimeZone::timestamp_opt(&Utc, 1_800_000_000 + secs, 0).unwrap()
    }

    /// Set a path's atime/mtime to `1_800_000_000 + epoch_offset_secs` — a
    /// test-only helper built on `libc::utimes` (already a dependency for the
    /// `flock` gate above) rather than pulling in a `filetime` dev-dependency
    /// for one call site.
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
        // Non-empty content so byte-reclaim assertions below are meaningful
        // (a real rlib/dep-graph file is never zero bytes).
        fs::write(path, b"fixture-payload").unwrap();
        set_mtime(path, secs);
    }

    fn candidate(name: &str, kind: EntryKind, age_secs: i64, size_bytes: u64) -> PruneCandidate {
        PruneCandidate {
            path: PathBuf::from(name),
            kind,
            newest_mtime: t(-age_secs),
            size_bytes,
        }
    }

    // ===================================================================
    // is_stale / plan_prune — the pure core
    // ===================================================================

    #[test]
    fn an_entry_older_than_the_threshold_is_stale() {
        let seven_days = 7 * 86_400;
        let c = candidate("a", EntryKind::IncrementalSession, seven_days + 1, 1);
        assert!(c.is_stale(t(0), 7));
    }

    #[test]
    fn an_entry_younger_than_the_threshold_is_kept() {
        let c = candidate("a", EntryKind::IncrementalSession, 3 * 86_400, 1);
        assert!(!c.is_stale(t(0), 7));
    }

    #[test]
    fn exactly_at_the_threshold_is_stale() {
        let seven_days = 7 * 86_400;
        let c = candidate("a", EntryKind::IncrementalSession, seven_days, 1);
        assert!(c.is_stale(t(0), 7));
    }

    #[test]
    fn threshold_zero_prunes_everything_not_from_the_future() {
        // Documented behavior (module docs / `PruneCandidate::is_stale`):
        // threshold=0 means no age floor at all.
        let c = candidate("a", EntryKind::IncrementalSession, 1, 1);
        assert!(c.is_stale(t(0), 0));
        let brand_new = candidate("b", EntryKind::IncrementalSession, 0, 1);
        assert!(brand_new.is_stale(t(0), 0), "age 0 is still >= threshold 0");
    }

    #[test]
    fn plan_prune_separates_stale_from_fresh_and_reports_accurate_bytes() {
        let candidates = vec![
            candidate("old-incr", EntryKind::IncrementalSession, 10 * 86_400, 100),
            candidate("new-incr", EntryKind::IncrementalSession, 1 * 86_400, 200),
            candidate("old-deps", EntryKind::DepsFile, 8 * 86_400, 50),
            candidate("new-deps", EntryKind::DepsFile, 2 * 86_400, 400),
        ];
        let plan = plan_prune(&candidates, t(0), 7);
        assert_eq!(plan.remove.len(), 2);
        assert_eq!(plan.keep.len(), 2);
        assert_eq!(plan.bytes_to_remove(), 150);
        assert!(plan.summary().contains("1 incremental session"));
        assert!(plan.summary().contains("1 deps file"));
    }

    #[test]
    fn empty_plan_summarizes_as_nothing() {
        assert_eq!(PrunePlan::default().summary(), "nothing");
    }

    // ===================================================================
    // scan — mixed-age fixture (issue's Test Plan bullet 1)
    // ===================================================================

    #[test]
    fn scan_finds_incremental_sessions_and_deps_files_with_mixed_ages() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // incremental/<crate-hash>/s-<old>-working/{dep-graph.bin}
        touch_with_mtime(
            &root.join("debug/incremental/loom_daemon-abc/s-old-working/dep-graph.bin"),
            -10 * 86_400,
        );
        // A fresh session for the same crate.
        touch_with_mtime(
            &root.join("debug/incremental/loom_daemon-abc/s-new-working/dep-graph.bin"),
            -1 * 86_400,
        );
        // deps/ files, mixed age.
        touch_with_mtime(&root.join("debug/deps/libfoo-old.rlib"), -9 * 86_400);
        touch_with_mtime(&root.join("debug/deps/libfoo-new.rlib"), -1 * 86_400);

        let scanned = scan(root).unwrap();
        // 2 incremental session dirs + 2 deps files.
        let incr: Vec<_> = scanned
            .candidates
            .iter()
            .filter(|c| c.kind == EntryKind::IncrementalSession)
            .collect();
        let deps: Vec<_> = scanned
            .candidates
            .iter()
            .filter(|c| c.kind == EntryKind::DepsFile)
            .collect();
        assert_eq!(incr.len(), 2, "{:?}", scanned.candidates);
        assert_eq!(deps.len(), 2, "{:?}", scanned.candidates);

        let plan = plan_prune(&scanned.candidates, t(0), 7);
        assert_eq!(plan.remove.len(), 2, "only the two >=7-day-old entries");
        let removed_names: Vec<String> = plan
            .remove
            .iter()
            .map(|c| c.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(removed_names.iter().any(|n| n == "s-old-working"));
        assert!(removed_names.iter().any(|n| n == "libfoo-old.rlib"));
    }

    #[test]
    fn scan_of_a_missing_target_dir_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let scanned = scan(&missing).unwrap();
        assert!(scanned.candidates.is_empty());
    }

    #[test]
    fn scan_of_an_empty_target_dir_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let scanned = scan(tmp.path()).unwrap();
        assert!(scanned.candidates.is_empty());
    }

    // ===================================================================
    // Recursive-mtime freshness: a nested rewrite, not a new entry, must
    // still count as "fresh" even though the directory's OWN mtime is old
    // (module docs' "age alone" gate).
    // ===================================================================

    #[test]
    fn a_rewritten_nested_file_keeps_the_whole_session_dir_fresh() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let session_dir = root.join("debug/incremental/loom_daemon-abc/s-x-working");
        let nested_file = session_dir.join("dep-graph.bin");
        touch_with_mtime(&nested_file, -10 * 86_400);
        // Age the session dir's own mtime too (creation already made it old).
        set_mtime(&session_dir, -10 * 86_400);

        // Now simulate a long-running build rewriting the existing file's
        // CONTENT (no new directory entry, so the dir's own mtime does not
        // move) shortly before the scan.
        touch_with_mtime(&nested_file, -1);

        let scanned = scan(root).unwrap();
        let session = scanned
            .candidates
            .iter()
            .find(|c| c.kind == EntryKind::IncrementalSession)
            .unwrap();
        assert!(
            !session.is_stale(t(0), 7),
            "a recently-rewritten nested file must keep the session fresh, got age vs {:?}",
            session.newest_mtime
        );
    }

    // ===================================================================
    // cargo_lock_is_held — the global build-in-progress gate
    // ===================================================================

    #[test]
    fn no_cargo_lock_file_reads_as_not_held() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!cargo_lock_is_held(tmp.path()));
    }

    #[test]
    fn an_unlocked_cargo_lock_file_reads_as_not_held() {
        let tmp = tempfile::tempdir().unwrap();
        File::create(tmp.path().join(".cargo-lock")).unwrap();
        assert!(!cargo_lock_is_held(tmp.path()));
    }

    #[test]
    fn a_held_exclusive_lock_is_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join(".cargo-lock");
        let file = File::create(&lock_path).unwrap();
        assert!(try_lock_exclusive(&file).unwrap(), "must acquire cleanly first");

        // Held by `file` (in this same process, a distinct fd) — flock is
        // per-open-file-description, so a second independent open+lock
        // attempt correctly contends with it.
        assert!(cargo_lock_is_held(tmp.path()));

        unlock(&file).unwrap();
        assert!(!cargo_lock_is_held(tmp.path()));
    }

    // ===================================================================
    // run() end-to-end — dry-run vs real, and the build-in-progress defer
    // ===================================================================

    #[test]
    fn dry_run_reports_the_plan_but_removes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch_with_mtime(&root.join("debug/incremental/c-abc/s-old-working/x.bin"), -10 * 86_400);

        let report = run(root, 7, true, t(0));
        assert!(report.dry_run);
        assert!(!report.build_in_progress);
        assert_eq!(report.plan.remove.len(), 1);
        assert!(report.removed.is_empty());
        assert_eq!(report.bytes_reclaimed(), 0);
        assert!(report.bytes_reclaimable() > 0);
        assert!(
            root.join("debug/incremental/c-abc/s-old-working").exists(),
            "dry run must not delete anything"
        );
    }

    #[test]
    fn a_real_run_removes_only_stale_entries_and_reports_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch_with_mtime(&root.join("debug/incremental/c-abc/s-old-working/x.bin"), -10 * 86_400);
        touch_with_mtime(&root.join("debug/incremental/c-abc/s-new-working/x.bin"), -1 * 86_400);

        let report = run(root, 7, false, t(0));
        assert!(!report.dry_run);
        assert_eq!(report.removed.len(), 1);
        assert!(report.bytes_reclaimed() > 0);
        assert!(
            !root.join("debug/incremental/c-abc/s-old-working").exists(),
            "the stale session must be gone"
        );
        assert!(
            root.join("debug/incremental/c-abc/s-new-working").exists(),
            "the fresh session must survive"
        );
    }

    #[test]
    fn a_held_build_lock_defers_the_entire_pass_even_for_old_looking_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch_with_mtime(&root.join("debug/incremental/c-abc/s-old-working/x.bin"), -30 * 86_400);
        touch_with_mtime(&root.join("debug/deps/libfoo.rlib"), -30 * 86_400);

        let lock_path = root.join(".cargo-lock");
        let lock_file = File::create(&lock_path).unwrap();
        assert!(try_lock_exclusive(&lock_file).unwrap());

        let report = run(root, 7, false, t(0));
        assert!(report.build_in_progress);
        assert!(report.removed.is_empty(), "nothing may be removed mid-build");
        assert!(root.join("debug/incremental/c-abc/s-old-working").exists());
        assert!(root.join("debug/deps/libfoo.rlib").exists());
        assert!(report.reason().contains("deferring"));

        unlock(&lock_file).unwrap();
    }

    #[test]
    fn a_held_build_lock_also_defers_a_dry_run() {
        // A dry run should still report "would be deferred", not a
        // misleadingly optimistic reclaim estimate.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch_with_mtime(&root.join("debug/incremental/c-abc/s-old-working/x.bin"), -30 * 86_400);
        let lock_file = File::create(root.join(".cargo-lock")).unwrap();
        assert!(try_lock_exclusive(&lock_file).unwrap());

        let report = run(root, 7, true, t(0));
        assert!(report.build_in_progress);
        assert!(report.removed.is_empty());

        unlock(&lock_file).unwrap();
    }

    #[test]
    fn threshold_zero_removes_everything_when_unlocked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch_with_mtime(&root.join("debug/deps/libfoo.rlib"), -1);

        let report = run(root, 0, false, t(0));
        assert_eq!(report.removed.len(), 1);
        assert!(!root.join("debug/deps/libfoo.rlib").exists());
    }

    #[test]
    fn a_missing_target_dir_is_a_clean_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let report = run(&missing, 7, false, t(0));
        assert!(!report.build_in_progress);
        assert!(report.removed.is_empty());
        assert!(report.plan.remove.is_empty());
    }

    #[test]
    fn reason_mentions_dry_run_and_reclaim_estimate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch_with_mtime(&root.join("debug/deps/libfoo.rlib"), -30 * 86_400);
        let report = run(root, 7, true, t(0));
        assert!(report.reason().contains("DRY RUN"));
    }
}
