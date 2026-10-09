//! Standing down for repos the fleet captain reports star-free (W12 part 2).
//!
//! Every pass's evaluator lists every operator label of every managed repo,
//! and for a repo with no open starred issue that is all it does: it returns
//! no rows before any other read. (The level step after it, #10307, lists
//! its own labels in every managed repo; standing down does not touch it.) With `fleet.captainGauges.starFacts` the
//! fleet captain makes those listings once for the fleet and publishes, per
//! repo, how many open starred issues it found
//! ([`crate::observability::captain_gauges::facts`]). This module decides,
//! per repo and per pass, whether this host may skip its evaluator on the
//! strength of that.
//!
//! # What standing down means
//!
//! Exactly what the local pass does for a repo whose starred listing is
//! empty: no rows, an empty inheritance list published for the root, a
//! successful pass. Nothing is taken from the captain except permission to
//! skip a pass whose output is empty. A repo the captain reports as having
//! any starred issue is evaluated here, in full, as before: landing rows read
//! this host's queue and pool, inheritance is this host's dispatch input, and
//! escalations go through the marker dedupe every evaluating host shares.
//!
//! # When the captain's "no star here" is believed
//!
//! All of:
//!
//! - the fact is fresh and from the declared captain, judged at this pass's
//!   own clock (the caller hands in only such facts, see
//!   [`crate::observability::captain_gauges::star_free`]);
//! - this host's last work-finder tick shows no starred row for the repo;
//! - the captain's listing is later, by [`EVIDENCE_SLACK`], than this host's
//!   own latest evidence of a star there: a local pass that found one, a tick
//!   row, or a star intent this host applied.
//!
//! Anything else (no fact, a stale one, an uncovered repo, newer local
//! evidence) is the local pass, as before. There is no third outcome.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};

use super::task::RepoInput;

/// How much later than this host's own evidence of a star the captain's
/// listing must be before its "no star here" is believed: clock skew between
/// the two hosts plus the forge's listing lag after a label write.
pub const EVIDENCE_SLACK: Duration = Duration::seconds(300);

/// The captain's star-free reports for this pass, and this host's own
/// evidence across passes.
#[derive(Debug, Default)]
pub struct StandDown {
    /// Lowercased slug -> the captain's `as_of` for "no open starred issue".
    free: HashMap<String, DateTime<Utc>>,
    /// Lowercased slug -> when this host last had evidence of a star there.
    seen: HashMap<String, DateTime<Utc>>,
}

impl StandDown {
    /// Replace the captain's reports with this pass's (already fresh).
    pub fn set_free(&mut self, free: HashMap<String, DateTime<Utc>>) {
        self.free = free;
    }

    /// Record evidence, at `at`, that `slug` has a starred issue.
    pub fn note_star(&mut self, slug: &str, at: DateTime<Utc>) {
        let seen = self.seen.entry(slug.to_ascii_lowercase()).or_insert(at);
        *seen = (*seen).max(at);
    }

    /// Whether this pass at `now` leaves `repo` to the captain's report.
    pub fn stands_down(&mut self, repo: &RepoInput, now: DateTime<Utc>) -> bool {
        if repo.tick_rows.iter().any(|row| row.operator_priority) {
            self.note_star(&repo.slug, now);
        }
        let slug = repo.slug.to_ascii_lowercase();
        let Some(as_of) = self.free.get(&slug) else {
            return false;
        };
        self.seen
            .get(&slug)
            .is_none_or(|seen| *as_of >= *seen + EVIDENCE_SLACK)
    }
}
