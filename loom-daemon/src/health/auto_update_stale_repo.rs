//! The `auto_update` section's wrong-repo-resolution escalation (Issue #8513).
//!
//! The daemon binary is released from exactly one project, so a release repo
//! that keeps naming a version OLDER than what is already installed is the
//! symptom of having asked the wrong repository — not of a healthy, current
//! host. On the fleet host behind #8513 a daemon whose workspace was a
//! *consumer* repo resolved that project's own latest release (`v0.11.0`)
//! against an installed `0.19.248` and logged the soft "no artifact for this
//! platform" every tick for hours. Nothing escalated, because nothing on the
//! `health`/`status` surfaces distinguished it from an unbuilt platform.
//!
//! This is a **separate finding** from the staleness rules in
//! [`super::assess_auto_update`]: those compare the running binary against
//! the SOURCE checkout's HEAD, which says nothing at all about what the
//! release-artifact path resolved. A host can be perfectly current on source
//! and still be permanently unable to roll onto a release.

use crate::types::DaemonStatusReport;

/// Consecutive stale-repo ticks past which the section escalates.
///
/// Not `1`: a single wrong answer can be forge jitter (a transient list read,
/// a partially-published release). At/above this, the loop has made no
/// progress across several consecutive checks while the repo it queried kept
/// naming something older than what is installed — which is "stuck" in
/// exactly the sense a terminal or backing-off loop is.
const TICK_WARN: u32 = 3;

/// The degraded summary for a host stalled on a wrong-repo resolution, or
/// `None` when the streak has not reached [`TICK_WARN`].
///
/// Names the repo that was queried and the two env vars that override the
/// resolution, because "which repo did it ask?" was precisely the question
/// the pre-#8513 log line and health output could not answer.
pub(super) fn stalled_summary(status: &DaemonStatusReport) -> Option<String> {
    if status.auto_update_stale_repo_ticks < TICK_WARN {
        return None;
    }
    Some(format!(
        "auto_update has made no progress for {} ticks — the release resolved from {} is OLDER \
         than the installed version (probable wrong-repo resolution; check \
         LOOM_DAEMON_UPDATE_GH_REPO / LOOM_MACHINE_CHECKOUT)",
        status.auto_update_stale_repo_ticks,
        status
            .auto_update_stale_repo
            .as_deref()
            .unwrap_or("<unknown>")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(ticks: u32, repo: Option<&str>) -> DaemonStatusReport {
        DaemonStatusReport {
            auto_update_enabled: true,
            auto_update_stale_repo_ticks: ticks,
            auto_update_stale_repo: repo.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn below_the_threshold_is_not_a_finding() {
        for ticks in 0..TICK_WARN {
            assert_eq!(
                stalled_summary(&status(ticks, Some("consumer-owner/consumer-repo"))),
                None,
                "{ticks} consecutive tick(s) must not escalate"
            );
        }
    }

    #[test]
    fn at_the_threshold_it_names_the_repo_and_both_overrides() {
        let why = stalled_summary(&status(TICK_WARN, Some("consumer-owner/consumer-repo")))
            .expect("must escalate");
        assert!(why.contains("no progress"), "{why}");
        assert!(why.contains("consumer-owner/consumer-repo"), "{why}");
        assert!(why.contains("LOOM_DAEMON_UPDATE_GH_REPO"), "{why}");
        assert!(why.contains("LOOM_MACHINE_CHECKOUT"), "{why}");
    }

    #[test]
    fn a_streak_with_no_recorded_repo_still_reports_rather_than_going_silent() {
        // The count is the finding; a missing name must not swallow it.
        let why = stalled_summary(&status(9, None)).expect("must escalate");
        assert!(why.contains("<unknown>"), "{why}");
        assert!(why.contains("9 ticks"), "{why}");
    }
}
