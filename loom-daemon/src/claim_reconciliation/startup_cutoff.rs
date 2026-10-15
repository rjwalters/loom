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
//! is demoted to the periodic pass's rule (the multi-hour age gate, which
//! keeps a fresh claim). Applying a label bumps the issue's `updatedAt`, so
//! every claim this process makes is on the kept side. A claim from before
//! the restart that something else touched since (a comment) is also kept and
//! falls to the age gate: the pre-#6615 behaviour, not a stranded claim.
//! An issue with no `updatedAt` keeps the #6615 behaviour.

use chrono::{DateTime, Utc};

use super::{BuildingIssue, ReclaimReason, ReconcileAction};

/// `decisions` with every [`ReclaimReason::NoRecordAtStartup`] on an issue
/// updated at or after `cutoff` turned into [`ReconcileAction::Keep`]. No
/// cutoff (a pass that is not the daemon's startup pass, or a caller outside
/// the daemon) leaves `decisions` unchanged.
#[must_use]
pub(super) fn demote_post_cutoff_claims(
    decisions: Vec<(u32, ReconcileAction)>,
    issues: &[BuildingIssue],
    cutoff: Option<DateTime<Utc>>,
    root: &std::path::Path,
) -> Vec<(u32, ReconcileAction)> {
    let Some(cutoff) = cutoff else {
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
                         pass — it was updated at {at}, after this daemon's startup pass began \
                         ({cutoff}), so it may be a claim this daemon just made; the periodic \
                         pass's age gate decides it (#12222)",
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
