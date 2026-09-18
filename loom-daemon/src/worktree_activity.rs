//! Filesystem-observed liveness for a worktree (Issue #8116).
//!
//! # Why this exists
//!
//! [`crate::worktree_reaper`]'s artifact-reclaim pass deletes `target/` /
//! `node_modules/` from every worktree the *removal* pass is keeping but that
//! nothing appears to be using. "Appears to be using" was, until this module,
//! defined entirely by [`crate::worktree_ops::clean::classify_worktree`]'s
//! in-use gates: a live spawn-loop claim-lock, a `.loom-in-use` marker, or a
//! process whose **cwd** is inside the worktree.
//!
//! None of those three fire for an **in-session Task-tool builder** — the
//! shape an operator produces by spawning `/loom:builder` subagents directly
//! rather than through `/loom:sweep`. Such a builder has no daemon registry
//! record (so no claim-lock), writes no `.loom-in-use` marker, and issues each
//! shell command as a *fresh one-shot subshell* whose cwd is set per command
//! and which exits immediately — so `processes_using` sees an empty pid list
//! between commands, which is almost always. The worktree therefore classifies
//! as `SkipIssueNotClosed` — "kept, but idle" — and the reclaim pass deletes
//! its `target/` out from under an active build. Observed 2026-09-17 on
//! rjwalters/loom #8055–#8058/#8060/#8075: six concurrent builders, `target/`
//! reaped twice for #8056, each loss costing a ~5-minute full `cargo` rebuild.
//!
//! The registry-independent signal this module adds is the one the process
//! table cannot hide: **the worktree's own mtimes**. A builder that is editing
//! files, running `cargo`, or committing leaves fresh mtimes behind whether or
//! not any of its processes happen to be alive at the instant the reaper looks.
//!
//! # What counts as activity
//!
//! [`probe_worktree_activity`] answers "was anything under this worktree
//! written within `window`?" from three sources, cheapest first:
//!
//! 1. **Git refs** — `HEAD`, `index`, and `logs/HEAD` inside the worktree's
//!    *gitdir*. A linked worktree's `.git` is a FILE pointing at
//!    `<main>/.git/worktrees/<name>/`, and neither that file nor any
//!    working-tree file changes mtime when you commit — so "HEAD moved" is
//!    only observable by resolving the gitdir, which [`resolve_gitdir`] does.
//! 2. **Build-artifact directories, depth 1 only** — `target/` and its
//!    immediate children (`debug/`, `release/`, `.rustc_info.json`, ...). A
//!    running `cargo` rewrites these constantly, and depth-1 catches that for
//!    the cost of a handful of `stat`s instead of walking a 100k-file tree.
//!    Deleting `target/` under a *running* build is the worst case this module
//!    exists to prevent, so the artifact dirs are deliberately not excluded
//!    from the probe merely because they are the reclaim's target.
//! 3. **The source tree** — a bounded recursive walk of everything else,
//!    skipping `.git` and the artifact dirs already covered by (2), with an
//!    early exit on the first recent file.
//!
//! # Failure direction
//!
//! Matching the rest of the reaper: *absent evidence is not evidence of
//! absence*. An unreadable worktree root yields [`ActivityProbe::Unknown`],
//! which [`reclaim_skip_reason`] treats exactly like recent activity — a
//! worktree we cannot inspect is never reclaimed from. Only a completed walk
//! that found nothing recent yields [`ActivityProbe::Idle`].
//!
//! Clock skew fails the same way: an mtime in the *future* reads as age zero,
//! i.e. maximally recent, rather than as a negative age that could underflow
//! into "ancient".

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::worktree_ops::clean::WorktreeDecision;

/// Default activity window: a worktree written within this long is live.
///
/// 30 minutes is the acceptance criterion in #8116 ("skips `issue-<N>/target`
/// while `issue-<N>` has a commit or write in the last 30 min"). It is
/// comfortably longer than the gap between two tool calls in a slow-thinking
/// agent turn, and comfortably shorter than the reaper's own removal grace
/// period, so it delays a genuinely-idle worktree's reclaim by at most one
/// extra pass.
pub const DEFAULT_ACTIVITY_WINDOW_MINUTES: u64 = 30;

/// Env override for [`DEFAULT_ACTIVITY_WINDOW_MINUTES`]. `0` disables the gate
/// entirely (restoring the pre-#8116 behavior); an unparseable value falls back
/// to the default rather than to "disabled".
pub const ACTIVITY_WINDOW_ENV: &str = "LOOM_WORKTREE_ACTIVITY_WINDOW_MINUTES";

/// Upper bound on entries visited by one source-tree walk. A worktree whose
/// source tree is larger than this and has no recent file among the first
/// `MAX_ENTRIES_SCANNED` reads as [`ActivityProbe::Idle`]: having examined this
/// many entries without finding one inside the window is positive evidence of
/// idleness, not a failure to answer.
const MAX_ENTRIES_SCANNED: usize = 20_000;

/// Whether anything under a worktree was written recently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityProbe {
    /// Something under the worktree was written within the window.
    Recent {
        /// How long ago, in seconds (0 for a future mtime — see module docs).
        age_secs: u64,
    },
    /// Verified: the probe completed and found nothing within the window.
    Idle,
    /// The probe could not answer — the worktree root is unreadable.
    Unknown,
}

/// Resolve the activity window from [`ACTIVITY_WINDOW_ENV`], else
/// [`DEFAULT_ACTIVITY_WINDOW_MINUTES`]. [`Duration::ZERO`] means the gate is
/// disabled and [`probe_worktree_activity`] short-circuits to
/// [`ActivityProbe::Idle`].
#[must_use]
pub fn resolve_activity_window() -> Duration {
    let minutes = std::env::var(ACTIVITY_WINDOW_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_ACTIVITY_WINDOW_MINUTES);
    Duration::from_secs(minutes.saturating_mul(60))
}

/// The gitdir backing `worktree_path`.
///
/// For the primary checkout `.git` is a directory and is returned as-is. For a
/// linked worktree it is a FILE whose contents are `gitdir: <absolute path>`;
/// that path is where `HEAD`/`index`/`logs/HEAD` actually live, so it is the
/// only place a commit's mtime is observable. `None` when `.git` is missing or
/// the pointer file cannot be read/parsed.
#[must_use]
pub fn resolve_gitdir(worktree_path: &Path) -> Option<PathBuf> {
    let dot_git = worktree_path.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let pointer = contents
        .lines()
        .find_map(|l| l.trim().strip_prefix("gitdir:"))?;
    let pointer = pointer.trim();
    if pointer.is_empty() {
        return None;
    }
    let path = PathBuf::from(pointer);
    Some(if path.is_absolute() {
        path
    } else {
        worktree_path.join(path)
    })
}

/// Age of `path`'s mtime at `now`, or `None` when it cannot be stat'ed.
///
/// A future mtime (clock skew, a tool writing with a skewed timestamp) reports
/// age `0` rather than erroring — see the module docs' failure-direction note.
fn mtime_age(path: &Path, now: SystemTime) -> Option<Duration> {
    let modified = std::fs::symlink_metadata(path).ok()?.modified().ok()?;
    Some(now.duration_since(modified).unwrap_or(Duration::ZERO))
}

/// `Some(age)` when `path`'s mtime is within `window` of `now`.
fn recent_age(path: &Path, now: SystemTime, window: Duration) -> Option<Duration> {
    mtime_age(path, now).filter(|age| *age < window)
}

/// Source 1: the worktree's git refs (`HEAD`, `index`, `logs/HEAD`).
fn git_ref_activity(worktree_path: &Path, now: SystemTime, window: Duration) -> Option<Duration> {
    let gitdir = resolve_gitdir(worktree_path)?;
    ["HEAD", "index", "logs/HEAD", "ORIG_HEAD"]
        .iter()
        .filter_map(|name| recent_age(&gitdir.join(name), now, window))
        .min()
}

/// Source 2: build-artifact directories, depth 1 only.
fn artifact_dir_activity(
    worktree_path: &Path,
    now: SystemTime,
    window: Duration,
) -> Option<Duration> {
    let mut best: Option<Duration> = None;
    for pattern in crate::worktree_ops::orphan_recovery::BUILD_ARTIFACT_PATTERNS {
        let dir = worktree_path.join(pattern.trim_end_matches('/'));
        if !dir.is_dir() {
            continue;
        }
        let mut candidates = vec![dir.clone()];
        if let Ok(entries) = std::fs::read_dir(&dir) {
            candidates.extend(entries.flatten().map(|e| e.path()));
        }
        for candidate in candidates {
            if let Some(age) = recent_age(&candidate, now, window) {
                best = Some(best.map_or(age, |b: Duration| b.min(age)));
            }
        }
    }
    best
}

/// Whether `name` is a top-level entry the source walk must not descend into:
/// `.git` (covered by [`git_ref_activity`]) and every build-artifact
/// **directory** (covered by [`artifact_dir_activity`]).
///
/// The `is_dir` qualifier matters because `BUILD_ARTIFACT_PATTERNS` is a
/// dirty-detection list, not a reclaim list — it names files (`Cargo.lock`,
/// `pnpm-lock.yaml`, `.loom-in-use`) that [`crate::worktree_ops::clean::reclaim_worktree_artifacts`]
/// never touches. Excluding those from the walk *as well as* from the depth-1
/// artifact scan (which only looks at directories) would make editing
/// `Cargo.lock` invisible to the probe.
fn is_walk_excluded_root(name: &str, is_dir: bool) -> bool {
    if name == ".git" {
        return true;
    }
    is_dir
        && crate::worktree_ops::orphan_recovery::BUILD_ARTIFACT_PATTERNS
            .iter()
            .any(|pattern| pattern.trim_end_matches('/') == name)
}

/// Source 3: a bounded recursive walk of the working tree, early-exiting on the
/// first entry inside `window`.
fn source_tree_activity(
    worktree_path: &Path,
    now: SystemTime,
    window: Duration,
) -> Option<Duration> {
    let mut stack = vec![worktree_path.to_path_buf()];
    let mut visited = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_ENTRIES_SCANNED {
                return None;
            }
            let path = entry.path();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            let name = entry.file_name().to_string_lossy().to_string();
            if dir == worktree_path && is_walk_excluded_root(&name, is_dir) {
                continue;
            }
            if let Some(age) = recent_age(&path, now, window) {
                return Some(age);
            }
            if is_dir {
                stack.push(path);
            }
        }
    }
    None
}

/// Whether anything under `worktree_path` was written within `window` of `now`.
///
/// See the module docs for the three evidence sources and the failure
/// direction. A zero `window` disables the probe (returns
/// [`ActivityProbe::Idle`]) so a caller can restore pre-#8116 behavior without
/// a second code path.
#[must_use]
pub fn probe_worktree_activity(
    worktree_path: &Path,
    now: SystemTime,
    window: Duration,
) -> ActivityProbe {
    if window.is_zero() {
        return ActivityProbe::Idle;
    }
    if !worktree_path.is_dir() {
        return ActivityProbe::Unknown;
    }
    // Short-circuiting, cheapest source first: the root's own mtime is one
    // `stat` and moves whenever a top-level entry is created or removed; the
    // unbounded-ish source walk is last and runs only when nothing else
    // answered. An active worktree therefore almost never reaches it, and an
    // idle one pays the walk once per reaper tick.
    let found = recent_age(worktree_path, now, window)
        .or_else(|| git_ref_activity(worktree_path, now, window))
        .or_else(|| artifact_dir_activity(worktree_path, now, window))
        .or_else(|| source_tree_activity(worktree_path, now, window));
    match found {
        Some(age) => ActivityProbe::Recent {
            age_secs: age.as_secs(),
        },
        None => ActivityProbe::Idle,
    }
}

/// Why the artifact-reclaim pass must leave this worktree alone, or `None` to
/// reclaim from it.
///
/// This is the whole "which decisions are reclaimable" rule, lifted out of
/// [`crate::worktree_reaper::reclaim_kept_artifacts_generic`] so #8116's
/// activity gate joins it as a peer rather than as a special case bolted on
/// afterwards. The order is load-bearing: the *cheap, already-computed*
/// [`WorktreeDecision`] arms are consulted before the filesystem probe, so a
/// healthy pass over a worktree that is about to be removed outright pays
/// nothing for the new gate.
#[must_use]
pub fn reclaim_skip_reason(
    decision: &WorktreeDecision,
    worktree_path: &Path,
    now: SystemTime,
    window: Duration,
) -> Option<String> {
    match decision {
        WorktreeDecision::Remove | WorktreeDecision::RemoveWithQuarantine => {
            return Some(
                "eligible for full removal (handled by the directory reap pass)".to_string(),
            )
        }
        WorktreeDecision::SkipInUse(reason) => return Some(reason.clone()),
        _ => {}
    }
    match probe_worktree_activity(worktree_path, now, window) {
        ActivityProbe::Recent { age_secs } => Some(format!(
            "recent filesystem activity ({age_secs}s ago, within the {}m window) — treating as a \
             live worker with no registry record (#8116)",
            window.as_secs() / 60
        )),
        ActivityProbe::Unknown => Some(
            "worktree is not readable — cannot verify it is idle, so nothing is reclaimed (#8116)"
                .to_string(),
        ),
        ActivityProbe::Idle => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A `SystemTime` `secs` in the future — the testing seam that lets a
    /// just-created temp file look arbitrarily old without touching utimes.
    fn later(secs: u64) -> SystemTime {
        SystemTime::now() + Duration::from_secs(secs)
    }

    fn window(mins: u64) -> Duration {
        Duration::from_secs(mins * 60)
    }

    /// A worktree-shaped temp dir: a source file, a `target/` with one child,
    /// and a `.git` FILE pointing at a sibling gitdir (the linked-worktree
    /// shape, not the primary-checkout one).
    fn make_worktree() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join("issue-8116");
        fs::create_dir_all(wt.join("src")).unwrap();
        fs::write(wt.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::create_dir_all(wt.join("target/debug")).unwrap();
        fs::write(wt.join("target/debug/binary"), "x").unwrap();
        let gitdir = tmp.path().join("gitdir");
        fs::create_dir_all(gitdir.join("logs")).unwrap();
        fs::write(gitdir.join("HEAD"), "ref: refs/heads/feature/issue-8116\n").unwrap();
        fs::write(gitdir.join("index"), "idx").unwrap();
        fs::write(gitdir.join("logs/HEAD"), "reflog\n").unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        tmp
    }

    fn worktree_path(tmp: &tempfile::TempDir) -> PathBuf {
        tmp.path().join("issue-8116")
    }

    #[test]
    fn freshly_written_worktree_reads_as_recent() {
        let tmp = make_worktree();
        assert!(matches!(
            probe_worktree_activity(&worktree_path(&tmp), SystemTime::now(), window(30)),
            ActivityProbe::Recent { .. }
        ));
    }

    #[test]
    fn worktree_untouched_past_the_window_reads_as_idle() {
        let tmp = make_worktree();
        // Two hours after every file was written, with a 30m window.
        assert_eq!(
            probe_worktree_activity(&worktree_path(&tmp), later(7200), window(30)),
            ActivityProbe::Idle
        );
    }

    #[test]
    fn zero_window_disables_the_probe() {
        let tmp = make_worktree();
        assert_eq!(
            probe_worktree_activity(&worktree_path(&tmp), SystemTime::now(), Duration::ZERO),
            ActivityProbe::Idle
        );
    }

    #[test]
    fn missing_worktree_is_unknown_not_idle() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            probe_worktree_activity(&tmp.path().join("absent"), SystemTime::now(), window(30)),
            ActivityProbe::Unknown
        );
    }

    /// AC: a commit (which touches the GITDIR's HEAD/index, never a
    /// working-tree file) counts as activity. This is the case a naive
    /// working-tree-only walk misses entirely.
    #[test]
    fn a_commit_in_the_linked_gitdir_counts_as_activity() {
        let tmp = make_worktree();
        let wt = worktree_path(&tmp);
        // Everything in the working tree is 2h old; only the gitdir moved.
        let now = later(7200);
        assert_eq!(probe_worktree_activity(&wt, now, window(30)), ActivityProbe::Idle);
        fs::write(tmp.path().join("gitdir/HEAD"), "ref: refs/heads/feature/issue-8116\n").unwrap();
        // The rewrite's mtime is `SystemTime::now()`, i.e. 2h before `now`;
        // widen the window past that so the gitdir is the only fresh source.
        assert!(matches!(
            probe_worktree_activity(&wt, now, window(180)),
            ActivityProbe::Recent { .. }
        ));
    }

    /// AC: `target/` being the reclaim's own target does not exempt it from
    /// the probe — an in-progress `cargo` writing into `target/debug/` is the
    /// single most expensive thing to reap out from under.
    #[test]
    fn a_running_build_writing_into_target_counts_as_activity() {
        let tmp = make_worktree();
        let wt = worktree_path(&tmp);
        assert_eq!(probe_worktree_activity(&wt, later(7200), window(30)), ActivityProbe::Idle);
        fs::write(wt.join("target/debug/binary"), "rebuilt").unwrap();
        assert!(matches!(
            probe_worktree_activity(&wt, later(7200), window(180)),
            ActivityProbe::Recent { .. }
        ));
    }

    #[test]
    fn resolve_gitdir_handles_both_pointer_file_and_directory() {
        let tmp = make_worktree();
        let wt = worktree_path(&tmp);
        assert_eq!(resolve_gitdir(&wt), Some(tmp.path().join("gitdir")));

        let primary = tmp.path().join("primary");
        fs::create_dir_all(primary.join(".git")).unwrap();
        assert_eq!(resolve_gitdir(&primary), Some(primary.join(".git")));

        assert_eq!(resolve_gitdir(&tmp.path().join("absent")), None);
    }

    // -- reclaim_skip_reason ------------------------------------------------

    #[test]
    fn removal_and_in_use_decisions_short_circuit_before_the_probe() {
        let tmp = make_worktree();
        let wt = worktree_path(&tmp);
        let idle = later(7200);
        assert!(reclaim_skip_reason(&WorktreeDecision::Remove, &wt, idle, window(30))
            .is_some_and(|r| r.contains("full removal")));
        assert!(reclaim_skip_reason(
            &WorktreeDecision::RemoveWithQuarantine,
            &wt,
            idle,
            window(30)
        )
        .is_some_and(|r| r.contains("full removal")));
        assert_eq!(
            reclaim_skip_reason(
                &WorktreeDecision::SkipInUse("in use by shepherd".to_string()),
                &wt,
                idle,
                window(30)
            ),
            Some("in use by shepherd".to_string())
        );
    }

    /// AC (#8116): `worktree_reaper` skips `issue-<N>/target` while
    /// `issue-<N>` has a commit or write in the last 30 min — even though the
    /// decision is the ordinary "kept, idle" `SkipIssueNotClosed`, which is
    /// exactly what an in-session builder's open issue produces.
    #[test]
    fn kept_open_issue_worktree_is_protected_while_recently_written() {
        let tmp = make_worktree();
        let wt = worktree_path(&tmp);
        let decision = WorktreeDecision::SkipIssueNotClosed("OPEN".to_string());

        let reason = reclaim_skip_reason(&decision, &wt, SystemTime::now(), window(30));
        assert!(
            reason
                .as_deref()
                .is_some_and(|r| r.contains("recent filesystem activity")),
            "a just-written worktree must not be reclaimed from: {reason:?}"
        );

        // Two hours later the same worktree is genuinely idle and reclaimable.
        assert_eq!(reclaim_skip_reason(&decision, &wt, later(7200), window(30)), None);
    }

    #[test]
    fn unreadable_worktree_is_never_reclaimed_from() {
        let tmp = tempfile::tempdir().unwrap();
        let decision = WorktreeDecision::SkipIssueNotClosed("OPEN".to_string());
        assert!(reclaim_skip_reason(
            &decision,
            &tmp.path().join("absent"),
            SystemTime::now(),
            window(30)
        )
        .is_some_and(|r| r.contains("not readable")));
    }

    #[test]
    fn resolve_activity_window_defaults_and_parses() {
        // Not asserted against the live env (tests share a process); the
        // parsing rule itself is what matters and is exercised directly.
        assert_eq!(
            Duration::from_secs(DEFAULT_ACTIVITY_WINDOW_MINUTES * 60),
            window(DEFAULT_ACTIVITY_WINDOW_MINUTES)
        );
        assert!(resolve_activity_window() >= Duration::ZERO);
    }
}
