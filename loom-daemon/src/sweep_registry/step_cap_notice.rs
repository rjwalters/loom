//! The visible reason behind a Builder step's repeat-cap cooldown (#12500).
//!
//! When the step ledger's repeat cap arms for `(item, builder)`, the item
//! leaves the draw for up to 24 h. The window itself is local and
//! peer-broadcast only, so a human watching the issue saw it go quiet with
//! no reason. [`SweepRegistry::note_builder_cap`] posts one marked comment per
//! arming window naming the cause, the run count and the release time. It is
//! a comment only: no label change, so the cap stays a time-bounded cooldown
//! and not a park that needs a human to release.
//!
//! The ledger re-arms the cap on every further run inside the window; the
//! notice is posted once while its window is live (kept in memory, so a
//! restart can repeat it once).

use chrono::{DateTime, Utc};

use super::SweepRegistry;

/// Marker on the one comment posted per repeat-cap window.
pub const STEP_CAP_COMMENT_MARKER: &str = "<!-- loom:step-repeat-cap (#12500) -->";

/// The comment body. Pure.
#[must_use]
pub fn cap_notice_body(runs: usize, window_hours: u64, until: DateTime<Utc>) -> String {
    format!(
        "{STEP_CAP_COMMENT_MARKER}\n\
         **Builder paused on this issue by the repeat cap.** {runs} Builder runs in the last \
         {window_hours} h ended without landing a PR (no-op, failed, or no outcome line), so the \
         daemon will not draw the Builder for it again until {until}. This is a time-bounded \
         cooldown, not a park: no label changed, and the issue is offered again afterwards. \
         A comment, a label change or a verdict from another role lifts it earlier.",
        until = until.format("%Y-%m-%d %H:%M UTC"),
    )
}

impl SweepRegistry {
    /// Whether a repeat-cap notice for `item` is due at `now`, recording the
    /// window it covers (`until`) when so. False while the last notice's
    /// window is still live.
    pub(crate) fn claim_cap_notice(
        &mut self,
        item: u32,
        until: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> bool {
        if self.step_cap_notices.get(&item).is_some_and(|u| *u > now) {
            return false;
        }
        self.step_cap_notices.insert(item, until);
        true
    }

    /// The Builder step on `item` armed the repeat cap until `until`: post the
    /// reason once per window. Best-effort and forge-less in tests.
    pub(crate) fn note_builder_cap(&mut self, item: u32, until: DateTime<Utc>, now: DateTime<Utc>) {
        if !self.claim_cap_notice(item, until, now) {
            return;
        }
        let cfg = self.step_ledger.config();
        let body = cap_notice_body(cfg.repeat_cap, cfg.repeat_window.as_secs() / 3600, until);
        log::info!("step_ledger: #{item} builder repeat cap armed until {until}");
        if self.config.skip_label_flip {
            return;
        }
        let issue_arg = item.to_string();
        let mut comment = vec![
            "issue",
            "comment",
            issue_arg.as_str(),
            "--body",
            body.as_str(),
        ];
        let repo_flag = crate::claim_reconciliation::gh_call::loom_repo_flag();
        comment.extend(repo_flag.iter().map(String::as_str));
        if let Err(e) = self.gh_write("step_cap.comment", comment) {
            log::debug!("step_ledger: repeat-cap comment for #{item} failed: {e}");
        }
    }
}

#[cfg(test)]
#[path = "step_cap_notice_tests.rs"]
mod tests;
