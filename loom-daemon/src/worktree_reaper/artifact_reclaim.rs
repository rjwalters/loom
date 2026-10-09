//! The kept-worktree artifact reclaim engine: `target/`, `node_modules/`,
//! `.venv/`, `coverage/` removed from `issue-<N>` / `pr-<N>` worktrees that
//! are kept on disk but idle (#5187, #5939, #8116; reworked by #11071).
//!
//! # Two phases
//!
//! [`collect`] walks one class of worktree under one repo root and decides,
//! per worktree, whether its build artifacts may go: the shared removal
//! classifier ([`crate::worktree_ops::clean::classify_worktree`] or its
//! `pr-<N>` twin) and #8116's activity gate decide for the worktree, then
//! each artifact directory is vetted on its own (a real directory, not a
//! symlink at any level, inside the worktree, owned by this user). It
//! measures every artifact directory it finds, kept or not.
//!
//! [`remove`] takes the candidates from any number of [`collect`] calls,
//! largest first, and asks a stop condition before each one. The scheduled
//! tier never stops; the below-floor eager tier stops as soon as free space
//! is back above the floor ([`reclaim_idle_targets_below_floor`]). Just
//! before a directory is removed it must not be backing a running program
//! (#6127's [`ProtectedArtifact`] guard) and nothing may hold a file open
//! under it ([`crate::target_orphan_reclaim::production_open_handles`],
//! which refuses when it cannot look).
//!
//! # Logging (#11071)
//!
//! Every removal logs `category=worktree_target_idle` with the worktree,
//! the directory and its bytes. Every artifact directory left in place logs
//! why, with its bytes, at `info` when it holds anything. Until #11071 the
//! skip reasons were `debug`, so a 31-hour stretch in which every kept
//! worktree on loom-worker-1 was skipped left no trace in the daemon log.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::Utc;

use crate::target_orphan_reclaim::{
    current_euid, newest_mtime_and_size, owned_by_us, production_open_handles, vet_path_inside,
};
use crate::worktree_activity::{reclaim_skip_reason, resolve_activity_window};
use crate::worktree_ops::clean::{self, PrStatus, ProtectedArtifact, WorktreeDecision};
use crate::worktree_ops::safety::{find_processes_executing_within, LiveExecutable};

/// The category every removal log line carries, so #10985 can attribute it.
pub const CATEGORY: &str = "worktree_target_idle";

/// One artifact directory inside one kept worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDir {
    /// `"issue"` or `"pr"`.
    pub class: &'static str,
    /// The worktree's issue or PR number.
    pub num: u32,
    /// The worktree root.
    pub worktree: PathBuf,
    /// Top-level directory name, e.g. `"target"`.
    pub name: String,
    /// Bytes under it, not following symlinks (0 for a symlink).
    pub bytes: u64,
}

impl ArtifactDir {
    /// The directory itself.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.worktree.join(&self.name)
    }

    fn label(&self) -> String {
        format!("{}-{}/{}", self.class, self.num, self.name)
    }
}

/// What one artifact-reclaim pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReclaimReport {
    /// Worktree directories of the pass's class examined.
    pub scanned: usize,
    /// Worktree number -> artifact directory names removed (or, on a dry
    /// run, that would be), for worktrees that had at least one.
    pub reclaimed: Vec<(u32, Vec<String>)>,
    /// Worktrees left untouched as a whole, with why.
    pub skipped: Vec<(u32, String)>,
    /// Every artifact directory removed (or that would be), with its bytes.
    pub removed: Vec<ArtifactDir>,
    /// Artifact directories left in place, with why: their worktree was
    /// skipped, or the directory itself failed a gate.
    pub kept: Vec<(ArtifactDir, String)>,
    /// Eligible directories not removed because the stop condition fired.
    pub deferred: Vec<ArtifactDir>,
    /// Why the pass stopped early, when it did.
    pub stopped: Option<String>,
    /// Eligible directories whose removal failed.
    pub failed: Vec<(ArtifactDir, String)>,
    /// `removed` lists what would be removed; nothing was.
    pub dry_run: bool,
}

impl ReclaimReport {
    /// A compact one-line summary for the daemon log.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "scanned={} reclaimed={} skipped={}",
            self.scanned,
            self.reclaimed.len(),
            self.skipped.len()
        )
    }

    /// Bytes removed (or, on a dry run, that would be).
    #[must_use]
    pub fn bytes_freed(&self) -> u64 {
        self.removed.iter().map(|a| a.bytes).sum()
    }

    /// Bytes still held by artifact directories this pass kept or deferred.
    #[must_use]
    pub fn bytes_kept(&self) -> u64 {
        self.kept.iter().map(|(a, _)| a.bytes).sum::<u64>()
            + self.deferred.iter().map(|a| a.bytes).sum::<u64>()
    }

    /// [`Self::summary`] plus bytes and the per-directory counts.
    #[must_use]
    pub fn detail(&self) -> String {
        format!(
            "{} removed_dirs={} bytes_freed={} kept_dirs={} deferred_dirs={} bytes_kept={} \
             failed={}",
            self.summary(),
            self.removed.len(),
            self.bytes_freed(),
            self.kept.len(),
            self.deferred.len(),
            self.bytes_kept(),
            self.failed.len()
        )
    }

    /// Fold another pass's report into this one (the cross-root eager pass).
    pub fn absorb(&mut self, other: Self) {
        self.scanned += other.scanned;
        self.reclaimed.extend(other.reclaimed);
        self.skipped.extend(other.skipped);
        self.removed.extend(other.removed);
        self.kept.extend(other.kept);
        self.deferred.extend(other.deferred);
        self.failed.extend(other.failed);
        self.stopped = self.stopped.take().or(other.stopped);
        self.dry_run |= other.dry_run;
    }
}

/// The two process probes [`remove`] applies to a directory just before
/// removing it, injected so tests need no real process table.
pub struct ArtifactProbes<'a> {
    /// `Some(true)` something holds a file open under the path, `Some(false)`
    /// nothing does, `None` the probe could not look (kept).
    pub open_handles: &'a dyn Fn(&Path) -> Option<bool>,
    /// Processes whose executable image lives under the path (#6127).
    pub executing_within: &'a dyn Fn(&Path) -> Vec<LiveExecutable>,
    /// This process's effective uid: a directory owned by anyone else is kept.
    pub euid: u32,
}

/// The production [`ArtifactProbes`].
#[must_use]
pub fn production_artifact_probes() -> ArtifactProbes<'static> {
    ArtifactProbes {
        open_handles: &production_open_handles,
        executing_within: &find_processes_executing_within,
        euid: current_euid(),
    }
}

/// One naming class of worktree and the classifier that decides for it.
pub struct WorktreeClass<'a> {
    /// `"issue"` or `"pr"`, as in the directory name.
    pub prefix: &'static str,
    /// Parses a directory name into the worktree's number.
    pub parse_name: &'a dyn Fn(&str) -> Option<u32>,
    /// The shared removal classifier, so "in use" never drifts from the
    /// removal pass's definition.
    pub classify: &'a dyn Fn(&Path, u32) -> WorktreeDecision,
}

/// The artifact directories present at the top of `worktree`: a directory,
/// or a symlink (reported so it can be kept with a reason). A regular file
/// that shares a pattern name (`Cargo.lock`) is never one.
fn artifact_dirs(worktree: &Path) -> Vec<(String, PathBuf, std::fs::Metadata)> {
    crate::worktree_ops::orphan_recovery::BUILD_ARTIFACT_PATTERNS
        .iter()
        .map(|p| p.trim_end_matches('/'))
        .filter_map(|name| {
            let path = worktree.join(name);
            let meta = std::fs::symlink_metadata(&path).ok()?;
            (meta.is_dir() || meta.file_type().is_symlink()).then(|| (name.to_string(), path, meta))
        })
        .collect()
}

fn measured(meta: &std::fs::Metadata, path: &Path) -> u64 {
    if meta.file_type().is_symlink() {
        return 0;
    }
    newest_mtime_and_size(path).map_or(0, |(_, bytes)| bytes)
}

/// Phase one over one class of worktree under `repo_root`: record why each
/// skipped worktree (and each of its artifact directories) is kept, and
/// return the artifact directories that may be removed.
pub fn collect(
    repo_root: &Path,
    class: &WorktreeClass<'_>,
    now: SystemTime,
    activity_window: Duration,
    euid: u32,
    report: &mut ReclaimReport,
) -> Vec<ArtifactDir> {
    let mut candidates = Vec::new();
    for entry in super::enumerate_worktree_dirs(repo_root) {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(num) = (class.parse_name)(&name) else {
            continue;
        };
        report.scanned += 1;
        let worktree = entry.path().canonicalize().unwrap_or_else(|_| entry.path());
        let dir = |name: &str, bytes: u64| ArtifactDir {
            class: class.prefix,
            num,
            worktree: worktree.clone(),
            name: name.to_string(),
            bytes,
        };

        let skip = if class.prefix == "issue"
            && crate::tokens_pool::private_workspace::export::has_issue(repo_root, num)
        {
            Some(
                "private-workspace issue (.loom/private-jobs): its worktree is not ours to trim"
                    .into(),
            )
        } else {
            let decision = (class.classify)(&worktree, num);
            reclaim_skip_reason(&decision, &worktree, now, activity_window)
        };
        if let Some(reason) = skip {
            for (name, path, meta) in artifact_dirs(&worktree) {
                report
                    .kept
                    .push((dir(&name, measured(&meta, &path)), reason.clone()));
            }
            report.skipped.push((num, reason));
            continue;
        }

        for (name, path, meta) in artifact_dirs(&worktree) {
            if let Err(why) = vet_path_inside(&worktree, &path) {
                report
                    .kept
                    .push((dir(&name, 0), format!("not reclaimed: {why}")));
                continue;
            }
            if !owned_by_us(meta.uid(), euid) {
                report.kept.push((
                    dir(&name, measured(&meta, &path)),
                    format!("owned by uid {}, not this daemon's", meta.uid()),
                ));
                continue;
            }
            candidates.push(dir(&name, measured(&meta, &path)));
        }
    }
    candidates
}

/// A stop condition that never fires: the scheduled tier's.
#[must_use]
pub fn never_stop(_: &ArtifactDir) -> Option<String> {
    None
}

/// Phase two: remove `candidates` largest first, asking `stop` before each.
/// Once it answers, every remaining candidate is deferred.
pub fn remove(
    mut candidates: Vec<ArtifactDir>,
    dry_run: bool,
    probes: &ArtifactProbes<'_>,
    stop: &dyn Fn(&ArtifactDir) -> Option<String>,
    report: &mut ReclaimReport,
) {
    report.dry_run = dry_run;
    candidates.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path().cmp(&b.path())));
    let mut pending = candidates.into_iter();
    for artifact in pending.by_ref() {
        if let Some(why) = stop(&artifact) {
            report.stopped = Some(why);
            report.deferred.push(artifact);
            break;
        }
        let path = artifact.path();
        let holders = (probes.executing_within)(&path);
        if !holders.is_empty() {
            let reason = ProtectedArtifact {
                name: artifact.name.clone(),
                holders,
            }
            .reason();
            report.kept.push((artifact, reason));
            continue;
        }
        match (probes.open_handles)(&path) {
            Some(false) => {}
            Some(true) => {
                report
                    .kept
                    .push((artifact, "a process holds a file open under it".into()));
                continue;
            }
            None => {
                report.kept.push((
                    artifact,
                    "open-handle probe unavailable (no /proc, no lsof): cannot verify it is free"
                        .into(),
                ));
                continue;
            }
        }
        if dry_run {
            record_removal(report, artifact);
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => record_removal(report, artifact),
            Err(e) => report.failed.push((artifact, e.to_string())),
        }
    }
    report.deferred.extend(pending);
}

fn record_removal(report: &mut ReclaimReport, artifact: ArtifactDir) {
    match report
        .reclaimed
        .iter_mut()
        .find(|(n, _)| *n == artifact.num)
    {
        Some((_, names)) => names.push(artifact.name.clone()),
        None => report
            .reclaimed
            .push((artifact.num, vec![artifact.name.clone()])),
    }
    report.removed.push(artifact);
}

/// [`collect`] then [`remove`] over one class under one root.
#[must_use]
pub fn reclaim_kept(
    repo_root: &Path,
    class: &WorktreeClass<'_>,
    dry_run: bool,
    activity_window: Duration,
    probes: &ArtifactProbes<'_>,
    stop: &dyn Fn(&ArtifactDir) -> Option<String>,
) -> ReclaimReport {
    let mut report = ReclaimReport::default();
    let candidates =
        collect(repo_root, class, SystemTime::now(), activity_window, probes.euid, &mut report);
    remove(candidates, dry_run, probes, stop, &mut report);
    report
}

/// Log the below-floor cross-root pass ([`reclaim_idle_targets_below_floor`]).
pub fn log_idle_target_report(repo_root: &Path, report: &ReclaimReport) {
    log_report(repo_root, report, "below-floor issue/pr");
}

/// Log one pass over `repo_root` (`class`: `"issue"` or `"pr"`).
pub fn log_report(repo_root: &Path, report: &ReclaimReport, class: &str) {
    let holds_bytes = report.bytes_kept() > 0;
    if report.removed.is_empty() && report.failed.is_empty() && !holds_bytes {
        log::debug!(
            "worktree_reaper: {} {class}-<N> artifact reclaim: nothing to reclaim ({})",
            repo_root.display(),
            report.detail()
        );
    } else {
        log::info!(
            "worktree_reaper: {} {class}-<N> artifact reclaim: {} reclaimed={:?}",
            repo_root.display(),
            report.detail(),
            report.reclaimed
        );
    }
    for a in &report.removed {
        log::info!(
            "worktree_reaper: category={CATEGORY} worktree={} dir={} bytes={} dry_run={}",
            a.worktree.display(),
            a.name,
            a.bytes,
            report.dry_run
        );
    }
    for (a, why) in &report.kept {
        if a.bytes > 0 {
            log::info!("worktree_reaper: keeping {} (bytes={}): {why}", a.label(), a.bytes);
        } else {
            log::debug!("worktree_reaper: keeping {} (bytes=0): {why}", a.label());
        }
    }
    for a in &report.deferred {
        log::info!(
            "worktree_reaper: deferring {} (bytes={}): {}",
            a.label(),
            a.bytes,
            report.stopped.as_deref().unwrap_or("stopped")
        );
    }
    for (a, why) in &report.failed {
        log::warn!(
            "worktree_reaper: category={CATEGORY} could not remove {}: {why}",
            a.path().display()
        );
    }
    for (num, reason) in &report.skipped {
        log::debug!(
            "worktree_reaper: {} artifact reclaim skipping {class}-{num}: {reason}",
            repo_root.display()
        );
    }
}

// ============================================================================
// The below-floor eager tier, across every registered root (#11071)
// ============================================================================

/// Stop once the volume holding `artifact` has `floor_gb` free, or when free
/// space cannot be measured (never delete for a need nobody can see).
fn floor_reached(artifact: &ArtifactDir, floor_gb: u64) -> Option<String> {
    match crate::disk_headroom::path_free_gb(&artifact.worktree) {
        Some(free) if free >= floor_gb => {
            Some(format!("free space back above the floor ({free}G >= {floor_gb}G)"))
        }
        Some(_) => None,
        None => Some("free space unmeasurable — stopping rather than guessing".to_string()),
    }
}

/// The eager tier's idle-target pass: kept-worktree artifacts across **every
/// registered root**, largest first, stopping once free space is back above
/// `floor_gb`.
///
/// The eager tier's own root is the dispatch loop's probe root, which on a
/// fleet host is the daemon's checkout (`~/loom-daemon`) and holds no
/// worktrees, so before #11071 nothing below the floor ever reached a repo's
/// kept worktrees. This pass does, without the forge: the classifier runs
/// with the issue and PR state unknown, which leaves exactly its local
/// in-use gates (live claim, `.loom-in-use`, a process in the worktree) plus
/// the activity window, and can never decide to remove a worktree. A root
/// whose scheduled pass is in flight is skipped (#7512).
#[must_use]
pub fn reclaim_idle_targets_below_floor(fallback_root: &Path, floor_gb: u64) -> ReclaimReport {
    let roots = crate::workspace_registry::WorkspaceRegistry::load_default()
        .unwrap_or_default()
        .effective_roots(fallback_root);
    let stop = |a: &ArtifactDir| floor_reached(a, floor_gb);
    reclaim_idle_targets(&roots, false, &production_artifact_probes(), &stop)
}

/// `loom-daemon clean`'s view: every idle kept-worktree artifact directory
/// across `roots`, with no floor (so `--dry-run` lists all of them).
#[must_use]
pub fn clean_idle_targets(roots: &[PathBuf], dry_run: bool) -> ReclaimReport {
    reclaim_idle_targets(roots, dry_run, &production_artifact_probes(), &never_stop)
}

/// [`reclaim_idle_targets_below_floor`]'s body over explicit `roots`.
#[must_use]
pub fn reclaim_idle_targets(
    roots: &[PathBuf],
    dry_run: bool,
    probes: &ArtifactProbes<'_>,
    stop: &dyn Fn(&ArtifactDir) -> Option<String>,
) -> ReclaimReport {
    let window = resolve_activity_window();
    let now = SystemTime::now();
    let mut report = ReclaimReport::default();
    let mut candidates = Vec::new();
    let mut guards = Vec::new();
    for root in roots {
        let Some(guard) = super::try_enter_reclaim(root) else {
            log::debug!(
                "worktree_reaper: {} idle-target pass skipped: another reclaim pass is in flight",
                root.display()
            );
            continue;
        };
        guards.push(guard);
        let config = super::read_worktree_reaper_config(root);
        let opts = super::reaper_clean_options(super::resolve_grace_period(&config));
        let active = crate::worktree_ops::liveness::active_spawn_loop_issues(root);
        let registered = clean::registered_worktree_paths(root);
        let is_registered = clean::is_registered_worktree_probe(&registered);
        let unknown_state = |_: u32| "UNKNOWN".to_string();
        let no_close = |_: u32| None;
        let unknown_pr = |_: u32| PrStatus::Unknown;
        let unreachable = |_: u32| false;
        // An editable install only ever turns a decision into another
        // reclaimable skip, so the `pip` probe per worktree is not run here.
        let no_editables = |_: &Path| Vec::new();
        let probes_issue = clean::WorktreeProbes {
            editable_installs: &no_editables,
            ..clean::production_probes(
                &active,
                &unknown_state,
                &no_close,
                &unknown_pr,
                &unreachable,
                &is_registered,
                Utc::now(),
            )
        };
        let classify_issue =
            |p: &Path, n: u32| clean::classify_worktree(p, n, &opts, &probes_issue);
        let issue = WorktreeClass {
            prefix: "issue",
            parse_name: &crate::worktree_ops::naming::issue_from_worktree,
            classify: &classify_issue,
        };
        candidates.extend(collect(root, &issue, now, window, probes.euid, &mut report));

        let unreachable_pr = |_: &Path| false;
        let probes_pr = clean::PrWorktreeProbes {
            editable_installs: &no_editables,
            ..clean::production_pr_probes(&unknown_pr, &unreachable_pr, Utc::now())
        };
        let classify_pr = |p: &Path, n: u32| clean::classify_pr_worktree(p, n, &opts, &probes_pr);
        let pr = WorktreeClass {
            prefix: "pr",
            parse_name: &crate::worktree_ops::naming::pr_from_worktree,
            classify: &classify_pr,
        };
        candidates.extend(collect(root, &pr, now, window, probes.euid, &mut report));
    }
    remove(candidates, dry_run, probes, stop, &mut report);
    drop(guards);
    report
}

#[cfg(test)]
#[path = "artifact_reclaim_tests.rs"]
mod tests;
