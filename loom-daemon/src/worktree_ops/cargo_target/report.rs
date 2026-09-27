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
    /// One `LEVEL<TAB>message` record, or `None` for the two uninteresting
    /// outcomes [`Self::report_line`] also stays quiet about.
    ///
    /// This is the wire form `merge-pr.sh`'s post-merge cleanup replays through
    /// its own `success`/`info`/`warning` — the protocol `merge-pr
    /// delete-branch` and `merge-pr dirty-guard` established (issue #9153,
    /// paying #8458's portable-shell residue). The severities are the ones
    /// `lib/cargo-target-dir.sh`'s retired `loom_render_target_dir_record`
    /// chose: a reclaim is a success, a deliberate keep is informational, and
    /// anything that could not be done is a warning.
    #[must_use]
    pub fn report_record(&self) -> Option<(&'static str, String)> {
        let level = match self {
            Self::Inside(_) | Self::Absent(_) => return None,
            Self::Reclaimed { .. } => "SUCCESS",
            Self::Shared { .. } | Self::WouldReclaim { .. } => "INFO",
            Self::Refused { .. } | Self::Protected { .. } | Self::Failed { .. } => "WARNING",
        };
        self.report_line().map(|line| (level, line))
    }

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

#[cfg(test)]
mod tests {
    use super::TargetDirOutcome;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn the_quiet_outcomes_emit_no_record() {
        assert!(TargetDirOutcome::Inside(p("/w/target"))
            .report_record()
            .is_none());
        assert!(TargetDirOutcome::Absent(p("/ext/target"))
            .report_record()
            .is_none());
    }

    #[test]
    fn a_reclaim_is_a_success_and_a_keep_is_informational() {
        let reclaimed = TargetDirOutcome::Reclaimed {
            path: p("/ext/wt/issue-1"),
            size_human: "12M".to_string(),
        };
        let (level, message) = reclaimed.report_record().expect("reclaimed reports");
        assert_eq!(level, "SUCCESS");
        assert!(message.starts_with("Reclaimed redirected cargo target dir: /ext/wt/issue-1"));

        let shared = TargetDirOutcome::Shared {
            path: p("/ext/shared"),
            by: p("/repo/.loom/worktrees/issue-2"),
        };
        let (level, message) = shared.report_record().expect("shared reports");
        assert_eq!(level, "INFO");
        assert!(message.contains("still used by"));
    }

    #[test]
    fn everything_undone_is_a_warning() {
        for outcome in [
            TargetDirOutcome::Refused {
                path: p("/tmp"),
                reason: "too shallow".to_string(),
            },
            TargetDirOutcome::Protected {
                path: p("/ext/wt/issue-3"),
                holders: vec!["pid 42".to_string()],
            },
            TargetDirOutcome::Failed {
                path: p("/ext/wt/issue-4"),
                error: "permission denied".to_string(),
            },
        ] {
            let (level, _) = outcome.report_record().expect("reports");
            assert_eq!(level, "WARNING", "{outcome:?}");
        }
    }

    /// The record's message half must stay the SAME text `clean` and the reaper
    /// print — one grammar, three surfaces (this is the whole point of the
    /// renderer living here rather than in each caller).
    #[test]
    fn the_message_half_is_exactly_report_line() {
        let outcome = TargetDirOutcome::WouldReclaim {
            path: p("/ext/wt/issue-5"),
            size_human: "3.0G".to_string(),
        };
        let (level, message) = outcome.report_record().expect("reports");
        assert_eq!(level, "INFO");
        assert_eq!(Some(message), outcome.report_line());
    }
}
