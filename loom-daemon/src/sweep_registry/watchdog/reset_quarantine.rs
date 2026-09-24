//! Quarantine-stash a worktree's uncommitted state **before** the mid-build
//! watchdog resets it (Issue #8413).
//!
//! # The gap this closes
//!
//! [`SweepRegistry::clean_worktree`](super::SweepRegistry::clean_worktree) runs
//! `git reset --hard` + `git clean -fd` on a worktree it has concluded is
//! dead-sweep debris. Since #4449 it first logs a porcelain status + diffstat of
//! what it is about to destroy — a forensic trace, not a recovery path. If the
//! conclusion is wrong (and #4449, #4556, #4564 and #7612 are four separate
//! incidents where it was), the log tells you exactly what was lost and gives
//! you no way to get it back.
//!
//! Every *other* destructive reclaim in the daemon already had the stronger
//! contract: [`crate::worktree_ops::clean::quarantine_dirty_worktree`] (#6653)
//! pushes uncommitted **and untracked** work onto a `loom-quarantine:`-labelled
//! stash and hands back the stash commit sha, which
//! [`crate::quarantine_stash_status`] then surfaces and
//! [`crate::stash_retirement`] ages out. This module gives the watchdog's reset
//! the same contract, so a reset the watchdog *does* perform is recoverable with
//! `git stash apply <sha>` — the sha being printed at `warn` in the daemon log,
//! the one place an unattended operator will look.
//!
//! # Why this never refuses the reset
//!
//! The stash is best-effort by design. "Nothing to stash" is the ordinary case
//! for a `clean_worktree` call on a worktree whose dirt is already gone, and a
//! `git stash push` failure (a broken index, a detached-in-a-weird-way HEAD)
//! must not wedge the mid-build recovery this path exists to perform — the
//! decision to reset was made by [`super::midbuild_decision`] under its own
//! refuse-to-destroy vetoes, and re-litigating it here would give one flaky
//! `git` invocation the power to strand an issue. So this function reports what
//! happened and returns; the refusal logic lives upstream, where the evidence
//! is.
//!
//! Note that `git stash push --include-untracked` does **not** stash
//! *ignored* files, so a multi-GB `target/` is not dragged into the object
//! store — only the work a human could not reproduce.

use std::path::Path;

/// Push `worktree`'s uncommitted + untracked state onto a `loom-quarantine:`
/// stash before a destructive reset, logging the recovery command.
///
/// Returns the stash commit sha when one was created, `None` when there was
/// nothing to stash or the push failed (both non-fatal — see the module docs).
pub(crate) fn quarantine_before_reset(worktree: &Path, issue: u32) -> Option<String> {
    let label = format!("issue={issue} reason=midbuild-watchdog-reset");
    match crate::worktree_ops::clean::quarantine_dirty_worktree(worktree, &label) {
        Some(sha) => {
            log::warn!(
                "clean-worktree: quarantined the uncommitted/untracked state of {} (issue \
                 #{issue}) to stash {sha} BEFORE `git reset --hard` + `git clean -fd` (#8413) — \
                 recover it with `git -C {} stash apply {sha}`; it is also listed by \
                 `loom-daemon status` until retired.",
                worktree.display(),
                worktree.display()
            );
            Some(sha)
        }
        None => {
            log::info!(
                "clean-worktree: no quarantine stash created for {} (issue #{issue}) before the \
                 reset — nothing to stash, or `git stash push` failed (#8413). The reset proceeds \
                 either way; see the preceding `DISCARDING`/`discarding` line for what it covers.",
                worktree.display()
            );
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::make_dirty_git_worktree;
    use std::process::Command;
    use tempfile::tempdir;

    /// AC (#8413): a reset the watchdog performs leaves a recoverable stash
    /// ref behind — labelled `loom-quarantine:`, reachable by sha.
    #[test]
    fn a_dirty_worktree_is_quarantined_to_a_recoverable_stash() {
        let tmp = tempdir().unwrap();
        let wt = make_dirty_git_worktree(tmp.path(), 8413);
        assert!(wt.join("dirty.txt").exists());

        let sha = quarantine_before_reset(&wt, 8413).expect("a dirty worktree yields a stash");

        // The worktree is clean afterwards (the stash took the dirt with it)...
        let status = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .unwrap();
        assert!(
            status.stdout.iter().all(u8::is_ascii_whitespace),
            "the quarantine stash cleared the working tree: {:?}",
            String::from_utf8_lossy(&status.stdout)
        );

        // ...and the recorded sha really names a `loom-quarantine:` stash
        // commit whose tree still contains the discarded file.
        let subject = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["log", "-1", "--format=%s", &sha])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&subject.stdout).contains("loom-quarantine:"),
            "stash subject must carry the quarantine label: {:?}",
            String::from_utf8_lossy(&subject.stdout)
        );

        let applied = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["stash", "apply", &sha])
            .output()
            .unwrap();
        assert!(
            applied.status.success(),
            "the logged sha must be applyable: {}",
            String::from_utf8_lossy(&applied.stderr)
        );
        assert!(
            wt.join("dirty.txt").exists(),
            "applying the quarantine stash restores the destroyed work"
        );
    }

    /// A clean worktree yields no stash, and that is not an error — the reset
    /// still proceeds upstream.
    #[test]
    fn a_clean_worktree_yields_no_stash() {
        let tmp = tempdir().unwrap();
        let wt = make_dirty_git_worktree(tmp.path(), 8414);
        std::fs::remove_file(wt.join("dirty.txt")).unwrap();
        assert_eq!(quarantine_before_reset(&wt, 8414), None);
    }
}
