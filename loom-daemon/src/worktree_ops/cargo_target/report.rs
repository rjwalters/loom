//! Operator-facing rendering of a [`TargetDirOutcome`] (issue #8458 split out
//! of `cargo_target.rs`).
//!
//! Split from the decision logic so the parent module stays within the file-size
//! ratchet (`.loom/docs/file-size-policy.md`) while this issue adds the
//! per-worktree resolution branch there. The lines are rendered identically by
//! the interactive `clean` pass and the unattended reaper's log, so "why did disk
//! not get freed?" has the same answer in both — that is the whole contract, and
//! it is unchanged by the move.

use super::TargetDirOutcome;

impl TargetDirOutcome {
    /// One operator-facing line, or `None` for the two uninteresting outcomes
    /// that describe every unredirected host (`Inside` / `Absent`). Rendered
    /// identically by the interactive `clean` pass and the unattended reaper's
    /// log, so "why did disk not get freed?" has the same answer in both.
    #[must_use]
    pub fn report_line(&self) -> Option<String> {
        match self {
            Self::Inside(_) | Self::Absent(_) => None,
            Self::Refused { path, reason } => {
                Some(format!("Refusing to reclaim cargo target dir {} — {reason}", path.display()))
            }
            Self::Shared { path, by } => Some(format!(
                "Keeping redirected cargo target dir {} — still used by {}",
                path.display(),
                by.display()
            )),
            Self::Protected { path, holders } => Some(format!(
                "Keeping redirected cargo target dir {} — {} live process(es) [{}] still using \
                 it; the reclaim is deferred, not lost",
                path.display(),
                holders.len(),
                holders.join(", ")
            )),
            Self::WouldReclaim { path, size_human } => Some(format!(
                "Would reclaim redirected cargo target dir: {} ({size_human})",
                path.display()
            )),
            Self::Reclaimed { path, size_human } => Some(format!(
                "Reclaimed redirected cargo target dir: {} ({size_human})",
                path.display()
            )),
            Self::Failed { path, error } => Some(format!(
                "Could not reclaim redirected cargo target dir {} — {error}",
                path.display()
            )),
        }
    }
}
