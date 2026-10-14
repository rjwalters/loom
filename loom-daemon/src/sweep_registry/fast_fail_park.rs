//! Park an issue whose dispatches keep failing fast (Issue #11854).
//!
//! # The gap
//!
//! The #4485 ladder bounds how *often* a failing issue is re-dispatched, but it
//! only ever delays: at its old 900 s ceiling an issue that fails a minute in
//! still ran ~100 times a day. The insta-crash quarantine (#3939) does park,
//! but it counts a narrower set of deaths than the ladder arms on, and both
//! used a 60 s window that the observed loops (60–90 s per failure) sat just
//! outside. So neither brake stopped them.
//!
//! # The fix, and why it is not a second park mechanism
//!
//! The ladder arms on a 2-minute window ([`FAST_FAIL_WINDOW_SECS`]). The
//! quarantine's own 60 s insta-crash window is left alone on purpose: it also
//! decides whether the pre-flight classifier runs (#4386), and widening it
//! would relabel a 90 s pre-CLI death `preflight-*`, which feeds the
//! workspace tripwire and exempts the death from the quarantine, the PR-less
//! hold and this park.
//!
//! Only an **issue-attributable** fast failure (no progress, not an
//! environmental class, not an open-PR yield) is charged under
//! `DispatchBackoffCause::FastFailure`. That cause alone climbs to the 6 h
//! ceiling, and its streak is the park's counter: a streak only grows under
//! the cause that started it (#8865), so a lost lease race, a token-pool
//! death, an environmental death or an open-PR yield (each charged under a
//! 900 s-capped cause instead) never counts toward a park, and interrupts the
//! count. When that streak reaches the quarantine threshold (1 while on
//! probation), the ladder parks the issue *through the quarantine itself*: the same in-memory
//! entry (TTL, generation escalation, probation, `loom-daemon quarantine
//! clear`), the same `insta-crash quarantine` body park record (#10161), the
//! same `loom:blocked` edit and the same anchored comment marker — so the
//! startup reconciliation (#4110), the TTL release and the stale-blocked
//! tooling all treat it exactly as a quarantine. The comment names the
//! failure class.
//!
//! One park, never two: the ladder parks only when the issue is not already
//! quarantined, and the quarantine's own tally only parks when the issue is
//! not already quarantined (`record_terminal_outcome`). The reaper charges the
//! ladder before the quarantine tally, so when both cross on the same death
//! the ladder parks and the quarantine sees an existing entry. An issue held
//! by the PR-less retry hold (#7972) or the no-op hold (#10156) is already
//! parked by that writer and is left alone.
//!
//! # What never parks
//!
//! - An *environmental* class (`preflight-*`, `account-exhausted:*`,
//!   `no-usable-account`, `self-kill:*`, `execution-error`; see
//!   [`is_environmental_failure_class`]). The run broke, not
//!   the issue: parking every issue a broken pool touched would empty the
//!   queue. They back off under the transient cause, capped at 900 s.
//! - A clean exit that yielded to a verified open PR (#6350). Nothing failed;
//!   it backs off under the open-PR guard's cause, also capped at 900 s.
//! - Anything while the ladder or the quarantine is disabled.
//! - A `loom:frozen` issue gets the in-memory pause but no forge write
//!   (`record_daemon_hold` answers `Frozen`, #11743).

use super::*;
use crate::telemetry::disposition::is_environmental_failure_class;
use crate::telemetry::UNCLASSIFIED_CLASS;

/// The fast-fail window, in seconds (Issue #11854): a no-progress, non-clean
/// death this soon after dispatch arms the #4485 ladder. Two minutes, because
/// the failures that looped all day ended 60–90 s in, just past the 60 s
/// insta-crash window the ladder used to share with the quarantine. A slower
/// death stays the mid-build (#3895) / review-stall (#3910) watchdogs' remit.
pub const FAST_FAIL_WINDOW_SECS: i64 = 120;

/// Whether a death `duration_sec` after dispatch with `exit_code` is a fast
/// failure: inside the wider of [`FAST_FAIL_WINDOW_SECS`] and the configured
/// insta-crash window (`insta_crash_secs`), and not a clean exit.
#[must_use]
pub fn is_fast_failure(duration_sec: i64, exit_code: Option<i32>, insta_crash_secs: i64) -> bool {
    duration_sec < FAST_FAIL_WINDOW_SECS.max(insta_crash_secs) && exit_code != Some(0)
}

/// The class a fast failure is parked under: the pre-flight verdict, then the
/// crash classifier's, then the no-op loop kind, then
/// [`UNCLASSIFIED_CLASS`]. Empty strings count as absent. The same
/// precedence the outcome journal uses for `failure_class` (#8056, #10156).
#[must_use]
pub fn fast_fail_class(
    death_class: Option<&str>,
    classification: Option<&str>,
    noop_loop: Option<&str>,
) -> String {
    [death_class, classification, noop_loop]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|class| !class.is_empty())
        .unwrap_or(UNCLASSIFIED_CLASS)
        .to_string()
}

/// Whether the `consecutive`-th fast failure of class `class` parks at
/// `threshold` (Issue #11854). Pure, so the rule is testable on its own.
#[must_use]
pub fn fast_fail_parks(consecutive: u32, threshold: u32, class: &str) -> bool {
    threshold > 0 && consecutive >= threshold && !is_environmental_failure_class(class)
}

impl SweepRegistry {
    /// The class to park a fast failure of `sweep_id` under, given what the
    /// reaper classified (see [`fast_fail_class`]).
    pub(crate) fn fast_fail_class_for(
        &self,
        issue: u32,
        sweep_id: &str,
        death_class: Option<&str>,
        classification: Option<&str>,
    ) -> String {
        let noop_loop = self
            .noop_loop_kind(issue, sweep_id)
            .map(ParkKind::failure_class);
        fast_fail_class(death_class, classification, noop_loop)
    }

    /// Charge one fast or no-progress failure to `issue`'s #4485 ladder under
    /// the right cause, and park it when its issue-attributable streak reaches
    /// the threshold (Issue #11854).
    ///
    /// - `open_pr_yield`: the open-PR guard's cause (900 s cap), never parks.
    /// - an environmental `class`: the transient cause (900 s cap), never parks.
    /// - otherwise: [`DispatchBackoffCause::FastFailure`] (up to `max`), and
    ///   the park check.
    ///
    /// A dispatch that self-reported a no-op release (#8912) is charged
    /// nothing; its own cooldown already covers it.
    pub(crate) fn charge_fast_failure(
        &mut self,
        sweep_id: &str,
        issue: u32,
        class: &str,
        open_pr_yield: bool,
    ) {
        if self.noop_release_covers_dispatch(issue, sweep_id)
            || is_environmental_failure_class(class)
        {
            // Logs the #8912 carve-out and records nothing, or records the
            // transient cause.
            self.charge_dispatch_failure(sweep_id, issue);
        } else if open_pr_yield {
            self.record_open_pr_guard_backoff(issue);
        } else {
            self.record_fast_failure(issue);
            self.park_fast_failure(issue, class);
        }
    }

    /// The issue-attributable fast-failure streak the park counts: the ladder's
    /// streak when [`DispatchBackoffCause::FastFailure`] armed it, else 0.
    #[must_use]
    pub(crate) fn fast_failure_streak(&self, issue: u32) -> u32 {
        self.dispatch_backoff
            .get(&issue)
            .filter(|s| s.cause == DispatchBackoffCause::FastFailure)
            .map_or(0, |s| s.consecutive)
    }

    /// Park `issue` through the quarantine when its fast-failure streak has
    /// reached the threshold (Issue #11854). Returns `true` when it parked. See
    /// the module docs for the reconciliation with the quarantine's own tally.
    pub(crate) fn park_fast_failure(&mut self, issue: u32, class: &str) -> bool {
        if !self.dispatch_backoff_config.enabled || !self.quarantine_config.enabled {
            return false;
        }
        let consecutive = self.fast_failure_streak(issue);
        // Probation (vibesql#6639): a relapse after a release parks at once.
        let threshold = if self.quarantine_generation(issue) > 0 {
            1
        } else {
            self.quarantine_config.threshold
        };
        if !fast_fail_parks(consecutive, threshold, class) {
            return false;
        }
        if self.is_quarantined(issue)
            || self.prless_retry_held(issue)
            || self.noop_hold_applied(issue)
        {
            log::debug!(
                "sweep_registry: issue #{issue} reached {consecutive} fast failures ({class}) but \
                 is already parked — not parking it twice (#11854)"
            );
            return false;
        }
        self.enter_quarantine(issue);
        log::warn!(
            "sweep_registry: issue #{issue} PARKED (generation {}) after {consecutive} consecutive \
             fast failures, failure_class={class} — pausing re-dispatch for {}s (#4485, #11854). \
             Clear with `loom-daemon quarantine clear {issue}` or wait for the TTL.",
            self.quarantine_generation(issue),
            self.effective_quarantine_ttl(issue).as_secs()
        );
        self.apply_fast_fail_park_label(issue, consecutive, class);
        true
    }

    /// The forge half of [`Self::park_fast_failure`]: the quarantine's park
    /// writes, with a comment that names the failure class.
    fn apply_fast_fail_park_label(&self, issue: u32, count: u32, class: &str) {
        let body = format!(
            "{marker}: this issue's sweep failed fast {count} times in a row (failure class \
             `{class}`) — each attempt ended within {secs}s of dispatch without doing any real \
             work, so the dispatch backoff (#4485) parked it instead of retrying it around the \
             clock (#11854). Once the cause is fixed, clear the park with `loom-daemon quarantine \
             clear {issue}`, or wait for the {ttl}s TTL — both release the in-memory pause AND \
             restore `loom:issue`.{escalation}",
            marker = QUARANTINE_COMMENT_MARKER,
            secs = FAST_FAIL_WINDOW_SECS.max(self.quarantine_config.insta_crash_secs),
            ttl = self.effective_quarantine_ttl(issue).as_secs(),
            escalation = self.quarantine_escalation_note(issue),
        );
        self.write_quarantine_park(issue, &body);
    }

    /// Best-effort forge mutation for a quarantine park (#3939): the body park
    /// record (#10161), then `loom:blocked` in place of `loom:issue`, then the
    /// explanatory `comment`. Skipped when label flips are disabled, stopped
    /// before the label on a `gh` timeout, and skipped entirely for a
    /// `loom:frozen` issue (#11743). A `gh` failure is logged at debug and never
    /// affects the load-bearing in-memory quarantine.
    pub(crate) fn write_quarantine_park(&self, issue: u32, comment: &str) {
        if self.config.skip_label_flip {
            return;
        }
        if self.record_daemon_hold(issue, park_hold::QUARANTINE_HOLD_REASON)
            != Ok(park_hold::HoldGate::Proceed)
        {
            return;
        }
        // Counted as `quarantine.*` (#10089); the facade scopes each call to
        // the registry's workspace (#5401 GH_CONFIG_DIR). Bounded (Issue
        // #3973): this runs from `reap_once`, on the `ListSweeps` /
        // `GetSweepStatus` read path.
        let repo_flag = crate::claim_reconciliation::gh_call::loom_repo_flag();
        let issue_arg = issue.to_string();
        let mut edit = vec!["issue", "edit", &issue_arg, "--add-label", "loom:blocked"];
        edit.extend(["--remove-label", "loom:issue"]);
        edit.extend(repo_flag.iter().map(String::as_str));
        let timeout = reap_gh_timeout();
        match self.gh_write("quarantine.label", edit) {
            Ok(Some(_)) => {}
            Ok(None) => log::debug!(
                "sweep_registry: quarantine label edit for #{issue} exceeded {}s, killed (#3973)",
                timeout.as_secs()
            ),
            Err(e) => log::debug!("sweep_registry: quarantine label edit for #{issue} failed: {e}"),
        }
        let mut args = vec!["issue", "comment", &issue_arg, "--body", comment];
        args.extend(repo_flag.iter().map(String::as_str));
        match self.gh_write("quarantine.comment", args) {
            Ok(Some(_)) => {}
            Ok(None) => log::debug!(
                "sweep_registry: quarantine comment for #{issue} exceeded {}s, killed (#3973)",
                timeout.as_secs()
            ),
            Err(e) => log::debug!("sweep_registry: quarantine comment for #{issue} failed: {e}"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "fast_fail_park_tests.rs"]
mod tests;
