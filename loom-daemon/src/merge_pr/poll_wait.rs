//! The deadline-or-wait decision of `_wait_for_checks_then_sync_merge`'s two
//! "keep polling" arms (#8191 slice): a poll whose check-runs could not be
//! fetched, and a poll whose rollup still has pending checks.
//!
//! Before this port each arm in `merge-pr.sh` carried its own
//! `date +%s >= deadline` comparison, its own `exit 5` timeout narration (#8896)
//! and, for the pending arm, a `printf | wc -l` count of the pending names.
//! Both arms ask the same question - has the bounded wait run out? - and now
//! share one answer, so the two timeout texts cannot drift apart.
//!
//! The deadline comparison is `now >= deadline`, exactly the retired
//! `[[ "$(date +%s)" -ge "$deadline" ]]`. The clock stays the CALLER's (`--now`):
//! the retained shell suites model time by shadowing `date`, and a verb that read
//! its own clock would make the wait untestable.

use std::fmt;

/// Which "keep polling" arm is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The check-runs read failed (`fetch_rc != 0`); `rc` is that code.
    Unfetchable,
    /// The rollup read fine and has pending checks.
    Pending,
}

/// What the loop does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Sleep one poll interval, then poll again.
    Wait,
    /// The bounded wait ran out: the caller exits 5 (re-queue, #8896).
    Timeout,
}

/// Severity the caller narrates the line at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
}

/// One decision plus the exact line to narrate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub action: Action,
    pub level: Level,
    pub message: String,
}

/// The decision inputs. `timeout` and `interval` are passed through verbatim
/// (they are only ever interpolated into text, as the retired shell did).
pub struct Inputs<'a> {
    pub kind: Kind,
    pub pr: &'a str,
    pub now: i64,
    pub deadline: i64,
    pub timeout: &'a str,
    pub interval: &'a str,
    /// The failed fetch's return code (`Unfetchable` only).
    pub rc: &'a str,
    /// The pending check names, one per line (`Pending` only); `wc -l` counted
    /// newline bytes, so this does too.
    pub pending: &'a str,
}

/// Number of newline bytes - the retired `printf '%s\n' "$pending" | wc -l`.
#[must_use]
pub fn pending_count(pending_with_trailing_newline: &str) -> usize {
    pending_with_trailing_newline.matches('\n').count()
}

/// Decide the next step of the poll loop.
#[must_use]
pub fn decide(i: &Inputs<'_>) -> Decision {
    let timed_out = i.now >= i.deadline;
    let n = pending_count(i.pending);
    match (i.kind, timed_out) {
        (Kind::Unfetchable, true) => Decision {
            action: Action::Timeout,
            level: Level::Warning,
            message: format!(
                "Timed out after {}s waiting for check-runs to become fetchable for PR #{} \u{2014} exiting 5 (not merged, not a failure: re-queue). Re-run once the forge API is healthy, or raise LOOM_AUTO_MERGE_TIMEOUT.",
                i.timeout, i.pr
            ),
        },
        (Kind::Unfetchable, false) => Decision {
            action: Action::Wait,
            level: Level::Warning,
            message: format!(
                "Failed to fetch check-runs for PR #{} (rc={}); treating as still-pending and continuing to poll",
                i.pr, i.rc
            ),
        },
        (Kind::Pending, true) => Decision {
            action: Action::Timeout,
            level: Level::Warning,
            message: format!(
                "Timed out after {}s waiting for {} pending check(s) on PR #{} to complete \u{2014} exiting 5 (not merged, not a failure: re-queue). Re-run once CI settles, or raise LOOM_AUTO_MERGE_TIMEOUT.",
                i.timeout, n, i.pr
            ),
        },
        (Kind::Pending, false) => Decision {
            action: Action::Wait,
            level: Level::Info,
            message: format!(
                "PR #{}: {} check(s) still running; waiting {}s for CI (timeout {}s)...",
                i.pr, n, i.interval, i.timeout
            ),
        },
    }
}

impl fmt::Display for Decision {
    /// `LOOM-POLL-WAIT <WAIT|TIMEOUT> <info|warning> <message>` - one line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let a = match self.action {
            Action::Wait => "WAIT",
            Action::Timeout => "TIMEOUT",
        };
        let l = match self.level {
            Level::Info => "info",
            Level::Warning => "warning",
        };
        write!(f, "LOOM-POLL-WAIT {a} {l} {}", self.message)
    }
}

#[cfg(test)]
mod tests;
