//! The startup pass's zero-evidence reclaim applies only to claims older than
//! this daemon (Issue #12222).
//!
//! [`ReclaimReason::NoRecordAtStartup`] (#6615) reclaims a `loom:building`
//! claim that has no journal entry, no run-registry pid and no checkpoint,
//! on the startup pass only: right after a restart, that absence means a
//! dispatch the previous process flipped and never recorded. The work finder
//! used to wait for the whole startup pass before its first tick, so no claim
//! of *this* process could exist while the pass ran.
//!
//! Since #12222 the work finder waits at most a bounded time for the pass
//! (`work_finder::startup_gate`), because a fleet host with hundreds of
//! open-PR claims spent 20–40 minutes in it and dispatched nothing. A claim
//! this process makes after the pass started also has zero durable evidence
//! for a moment (the journal entry is written after the spawn), so the pass
//! could otherwise reclaim it and the next tick would dispatch it twice.
//!
//! The cutoff closes that window: the pass records when it started
//! ([`crate::daemon_startup_reconciliation::claim_cutoff`]), and a
//! `NoRecordAtStartup` decision on an issue updated at or after that instant
//! (less a clock margin, below) is demoted to the periodic pass's rule (the multi-hour age gate, which
//! keeps a fresh claim). Applying a label bumps the issue's `updatedAt`, so
//! every claim this process makes is on the kept side. A claim from before
//! the restart that something else touched since (a comment) is also kept and
//! falls to the age gate: the pre-#6615 behaviour, not a stranded claim.
//! An issue with no `updatedAt` keeps the #6615 behaviour.
//!
//! # Precision and clock skew (#12230)
//!
//! The cutoff is this host's clock, to the nanosecond; `updatedAt` is
//! GitHub's clock, to the second. Compared raw, a claim made in the same
//! second as the cutoff, or within the host's clock skew of it, could read as
//! older than the cutoff and be reclaimed. The default wait (90 s, plus the
//! #12073 jitter on a fleet host) keeps the work finder's first claim well
//! clear of the cutoff, but with `LOOM_STARTUP_RECONCILE_WAIT_SECS=0` on a
//! non-fleet host the only margin is how long the local passes take. So the
//! comparison uses [`effective_cutoff`]: the cutoff rounded down to the whole
//! second and moved back by [`CLOCK_MARGIN`]. The cost is on the other side:
//! a claim the previous process flipped in its last minute is no longer
//! reclaimed at once by the startup pass, and falls to the age gate instead,
//! which is the pre-#6615 behaviour for that claim, not a stranded one.

use chrono::{DateTime, SubsecRound, Utc};

use super::{BuildingIssue, ReclaimReason, ReconcileAction};

/// How far before the (whole-second) cutoff a claim is still treated as
/// possibly this daemon's own: GitHub's clock against this host's (#12230).
pub(super) const CLOCK_MARGIN_SECS: i64 = 60;

/// The instant the `updatedAt` comparison actually uses: `cutoff` rounded
/// down to the whole second, minus [`CLOCK_MARGIN_SECS`] (#12230).
#[must_use]
pub(super) fn effective_cutoff(cutoff: DateTime<Utc>) -> DateTime<Utc> {
    cutoff.trunc_subsecs(0) - chrono::Duration::seconds(CLOCK_MARGIN_SECS)
}

/// `decisions` with every [`ReclaimReason::NoRecordAtStartup`] on an issue
/// updated at or after [`effective_cutoff`] of `cutoff` turned into
/// [`ReconcileAction::Keep`]. No
/// cutoff (a pass that is not the daemon's startup pass, or a caller outside
/// the daemon) leaves `decisions` unchanged.
#[must_use]
pub(super) fn demote_post_cutoff_claims(
    decisions: Vec<(u32, ReconcileAction)>,
    issues: &[BuildingIssue],
    cutoff: Option<DateTime<Utc>>,
    root: &std::path::Path,
) -> Vec<(u32, ReconcileAction)> {
    let Some(cutoff) = cutoff.map(effective_cutoff) else {
        return decisions;
    };
    decisions
        .into_iter()
        .map(|(number, action)| {
            if action != ReconcileAction::Reclaim(ReclaimReason::NoRecordAtStartup) {
                return (number, action);
            }
            let updated = issues
                .iter()
                .find(|i| i.number == number)
                .and_then(|i| i.updated_at);
            match updated {
                Some(at) if at >= cutoff => {
                    log::info!(
                        "claim_reconciliation: not reclaiming #{number} in {} on the startup \
                         pass — it was updated at {at}, at or after {cutoff} (when this \
                         daemon's startup pass began, to the second, less a {CLOCK_MARGIN_SECS}s \
                         clock margin), so it may be a claim this daemon just made; the periodic pass's age gate decides it \
                         (#12222, #12230)",
                        root.display()
                    );
                    (number, ReconcileAction::Keep)
                }
                _ => (number, action),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn issue(number: u32, updated_at: Option<DateTime<Utc>>) -> BuildingIssue {
        BuildingIssue { number, updated_at }
    }

    const STARTUP: ReconcileAction = ReconcileAction::Reclaim(ReclaimReason::NoRecordAtStartup);

    #[test]
    fn a_claim_updated_after_the_cutoff_is_kept() {
        let cutoff = Utc::now();
        let issues = [issue(1, Some(cutoff + chrono::Duration::seconds(5)))];
        let out =
            demote_post_cutoff_claims(vec![(1, STARTUP)], &issues, Some(cutoff), Path::new("/r"));
        assert_eq!(out, vec![(1, ReconcileAction::Keep)]);
    }

    #[test]
    fn a_claim_updated_exactly_at_the_cutoff_is_kept() {
        let cutoff = Utc::now();
        let issues = [issue(1, Some(cutoff))];
        let out =
            demote_post_cutoff_claims(vec![(1, STARTUP)], &issues, Some(cutoff), Path::new("/r"));
        assert_eq!(out, vec![(1, ReconcileAction::Keep)]);
    }

    /// #12230: the boundary. The cutoff is mid-second on the host's clock;
    /// GitHub reports `updatedAt` to the second, possibly behind the host.
    /// The comparison point is the cutoff's whole second less the margin:
    /// a claim at exactly that point is kept, one second earlier is not.
    #[test]
    fn the_cutoff_rounds_down_to_the_second_and_subtracts_the_margin() {
        let cutoff = DateTime::parse_from_rfc3339("2026-10-15T12:00:00.700Z")
            .unwrap()
            .with_timezone(&Utc);
        let boundary = DateTime::parse_from_rfc3339("2026-10-15T11:59:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(effective_cutoff(cutoff), boundary);
        let second = chrono::Duration::seconds(1);
        let cases = [
            // Same second as the cutoff, earlier than it by sub-seconds.
            (1, cutoff.trunc_subsecs(0), ReconcileAction::Keep),
            // Within the margin: host clock ahead of GitHub's.
            (2, cutoff - chrono::Duration::seconds(30), ReconcileAction::Keep),
            (3, boundary, ReconcileAction::Keep),
            (4, boundary - second, STARTUP),
        ];
        let issues: Vec<_> = cases.iter().map(|&(n, at, _)| issue(n, Some(at))).collect();
        let decisions = cases.iter().map(|&(n, _, _)| (n, STARTUP)).collect();
        let out = demote_post_cutoff_claims(decisions, &issues, Some(cutoff), Path::new("/r"));
        let want: Vec<_> = cases.iter().map(|&(n, _, want)| (n, want)).collect();
        assert_eq!(out, want);
    }

    #[test]
    fn a_claim_from_before_the_restart_is_still_reclaimed() {
        let cutoff = Utc::now();
        let issues = [issue(1, Some(cutoff - chrono::Duration::minutes(3)))];
        let out =
            demote_post_cutoff_claims(vec![(1, STARTUP)], &issues, Some(cutoff), Path::new("/r"));
        assert_eq!(out, vec![(1, STARTUP)]);
    }

    #[test]
    fn no_updated_at_keeps_the_6615_behaviour() {
        let cutoff = Utc::now();
        let out = demote_post_cutoff_claims(
            vec![(1, STARTUP)],
            &[issue(1, None)],
            Some(cutoff),
            Path::new("/r"),
        );
        assert_eq!(out, vec![(1, STARTUP)]);
    }

    #[test]
    fn other_reasons_and_no_cutoff_are_untouched() {
        let cutoff = Utc::now();
        let fresh = Some(cutoff + chrono::Duration::seconds(5));
        let dead = ReconcileAction::Reclaim(ReclaimReason::DeadPid { pid: 7 });
        let out = demote_post_cutoff_claims(
            vec![(1, dead), (2, ReconcileAction::Keep)],
            &[issue(1, fresh), issue(2, fresh)],
            Some(cutoff),
            Path::new("/r"),
        );
        assert_eq!(out, vec![(1, dead), (2, ReconcileAction::Keep)]);
        let out = demote_post_cutoff_claims(
            vec![(1, STARTUP)],
            &[issue(1, fresh)],
            None,
            Path::new("/r"),
        );
        assert_eq!(out, vec![(1, STARTUP)]);
    }
}
