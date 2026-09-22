//! Wrong-repo-resolution tracking for the auto-update tick (Issue #8513).
//!
//! A resolved release whose tag is **older** than the installed version is not
//! the healthy "nothing to fetch" case: the daemon binary is released from one
//! project, so a release repo that keeps naming something older than what is
//! already running is the symptom of having asked the *wrong* repository. On
//! the fleet host behind #8513 that was a workspace whose `origin` is a
//! consumer repo whose own latest release was `v0.11.0`, against an installed
//! `0.19.248` — logged as the soft "no artifact for this platform" every tick
//! for hours while the host sat one release short of a feature it needed.
//!
//! This module owns the two pieces of that finding the tick needs and
//! `auto_update.rs` (over `.loom/docs/file-size-policy.md`'s threshold, and
//! therefore frozen) must not grow to hold: the consecutive-tick streak that
//! `loom-daemon health` escalates on, and the WARN wording that names the
//! repo actually queried.

/// A run of **consecutive** ticks that resolved a release older than the
/// installed version, with the repo the most recent one queried.
///
/// The streak — not a single occurrence — is what
/// [`crate::health::assess_auto_update`] escalates on: one wrong answer can
/// be forge jitter, while a sustained run means the loop is making no
/// progress at all. Any tick with a different outcome (including one where no
/// artifact resolved) [`Self::reset`]s it, so the count can never accumulate
/// across unrelated ticks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct StaleRepoStreak {
    ticks: u32,
    repo: Option<String>,
}

impl StaleRepoStreak {
    /// Count one more consecutive stale-repo tick, naming the repo queried.
    pub(super) fn record(&mut self, repo: String) {
        self.ticks = self.ticks.saturating_add(1);
        self.repo = Some(repo);
    }

    /// Drop the streak: this tick resolved something other than a stale repo.
    pub(super) fn reset(&mut self) {
        self.ticks = 0;
        self.repo = None;
    }

    /// Consecutive stale-repo ticks so far (`0` when the streak is broken).
    pub(super) fn ticks(&self) -> u32 {
        self.ticks
    }

    /// The repo the most recent stale-repo tick queried, or `None` once the
    /// streak has reset.
    pub(super) fn repo(&self) -> Option<String> {
        self.repo.clone()
    }
}

/// The WARN-worthy tick reason for a stale-repo resolution.
///
/// **Names the repo**, which is the whole point: the pre-#8513 line said only
/// that the latest release was older than installed, so an operator could not
/// tell a wrong repository from a project that legitimately had not cut a
/// release yet.
pub(super) fn warn_reason(artifact: &str, installed: &str, repo: &str) -> String {
    format!(
        "resolved release {artifact} from {repo} is OLDER than the installed {installed} — \
         probable wrong-repo resolution (queried {repo}); nothing to fetch"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_streak_accumulates_across_consecutive_ticks() {
        let mut s = StaleRepoStreak::default();
        for expected in 1..=4u32 {
            s.record("consumer-owner/consumer-repo".to_string());
            assert_eq!(s.ticks(), expected);
        }
        assert_eq!(s.repo().as_deref(), Some("consumer-owner/consumer-repo"));
    }

    #[test]
    fn a_reset_drops_both_the_count_and_the_repo() {
        // The repo must go too: reporting a name with a zero count would read
        // as "this is the repo we are stuck on" on a host that is fine.
        let mut s = StaleRepoStreak::default();
        s.record("consumer-owner/consumer-repo".to_string());
        s.reset();
        assert_eq!(s.ticks(), 0);
        assert_eq!(s.repo(), None);
    }

    #[test]
    fn a_fresh_streak_reports_nothing() {
        let s = StaleRepoStreak::default();
        assert_eq!(s.ticks(), 0);
        assert_eq!(s.repo(), None);
    }

    #[test]
    fn the_warn_reason_names_the_repo_twice_over_and_both_versions() {
        let why = warn_reason("0.1.0", "0.19.248", "consumer-owner/consumer-repo");
        assert!(why.contains("consumer-owner/consumer-repo"), "{why}");
        assert!(why.contains("0.1.0"), "{why}");
        assert!(why.contains("0.19.248"), "{why}");
        assert!(why.contains("OLDER"), "{why}");
        assert!(why.contains("wrong-repo"), "{why}");
    }
}
