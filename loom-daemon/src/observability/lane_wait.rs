//! `merge.lane_wait` facts (#13029 row 1): the dispatch tick's merge-in-flight
//! exclusions, turned into a per-PR enter/leave pair so a wait has a duration.
//!
//! The tick already knows, at [`crate::step_dispatch::driver::shape_pool`],
//! which merge steps it dropped and which PR blocked them. This keeps the
//! previous tick's view per repo in memory and emits only the change: no forge
//! read, no polling, nothing new on the draw path. See
//! [`crate::telemetry::kinds::merge_lane_wait`] for the record's semantics.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use crate::step_dispatch::item_draw::{Exclusion, Weighing};
use crate::sweep_registry::step_spawn::draw_claims::DrawClaims;
use crate::telemetry::kinds::merge_lane_wait::{left_as, phase, MergeLaneWaitRecord};
use crate::telemetry::provenance::Provenance;

/// One PR's standing wait.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wait {
    step: String,
    blocker: u32,
    started: DateTime<Utc>,
    lower_bound: bool,
}

/// One repo's waits, keyed by the waiting PR. `initialized` is set by the
/// first observation after the daemon started, so a wait first seen then is a
/// lower bound.
#[derive(Debug, Default)]
struct RepoWaits {
    initialized: bool,
    waits: BTreeMap<u32, Wait>,
}

static STATE: Mutex<BTreeMap<String, RepoWaits>> = Mutex::new(BTreeMap::new());

/// What one tick saw in one repo.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// `(waiting PR, step)` the tick dropped as merge-in-flight.
    pub waiting: Vec<(u32, &'static str)>,
    /// The merge in flight: `(PR, local)`.
    pub blocker: Option<(u32, bool)>,
    /// Targets still candidates in the repo (drawable or excluded for another
    /// reason) and not waiting.
    pub present: HashSet<u32>,
}

/// What `weighing` (after the claims were taken out of it) and `claims` say.
#[must_use]
pub fn seen_from(weighing: &Weighing, claims: &DrawClaims, local_host: &str) -> Seen {
    let blocker = claims
        .merge_in_flight
        .as_ref()
        .map(|(pr, host)| (*pr, host == local_host));
    let mut seen = Seen {
        blocker,
        ..Seen::default()
    };
    for c in &weighing.pool {
        seen.present.insert(c.target());
    }
    for e in &weighing.excluded_items {
        let target = e.item.target().unwrap_or_default();
        if e.exclusion == Exclusion::MergeInFlight {
            if let Some(step) = e.item.step() {
                seen.waiting.push((target, step.as_str()));
            }
        } else {
            seen.present.insert(target);
        }
    }
    seen
}

/// The records for one observation, updating `repo`'s standing waits. Pure
/// apart from `state`.
fn diff(
    repo: &str,
    state: &mut RepoWaits,
    seen: &Seen,
    now: DateTime<Utc>,
    loom: &Provenance,
) -> Vec<MergeLaneWaitRecord> {
    let repo = repo.to_ascii_lowercase();
    let mut out = Vec::new();
    let record =
        |pr: u32, ph: &str, step: &str, blocker: u32, local: bool, w: &Wait| MergeLaneWaitRecord {
            repo: repo.clone(),
            pr,
            phase: ph.to_string(),
            step: step.to_string(),
            blocker_pr: blocker,
            blocker_source: if local { "local" } else { "peer" }.to_string(),
            wait_started_at: w.started,
            wait_started_at_lower_bound: w.lower_bound,
            left_as: None,
            wait_secs: None,
            observed_at: now,
            loom: loom.clone(),
        };
    if let Some((blocker, local)) = seen.blocker {
        for (pr, step) in &seen.waiting {
            match state.waits.get(pr) {
                Some(w) if w.blocker == blocker => {}
                existing => {
                    // A new blocker keeps the wait's start: the PR has been
                    // waiting since it was first held back.
                    let wait = Wait {
                        step: (*step).to_string(),
                        blocker,
                        started: existing.map_or(now, |w| w.started),
                        lower_bound: existing.map_or(!state.initialized, |w| w.lower_bound),
                    };
                    out.push(record(*pr, phase::ENTER, step, blocker, local, &wait));
                    state.waits.insert(*pr, wait);
                }
            }
        }
    }
    let waiting: HashSet<u32> = seen.waiting.iter().map(|(pr, _)| *pr).collect();
    let (blocker, local) = seen.blocker.unwrap_or_default();
    let left: Vec<u32> = state
        .waits
        .keys()
        .copied()
        .filter(|pr| !waiting.contains(pr))
        .collect();
    for pr in left {
        let Some(wait) = state.waits.remove(&pr) else {
            continue;
        };
        let mut rec = record(
            pr,
            phase::LEAVE,
            &wait.step,
            wait.blocker,
            local && wait.blocker == blocker,
            &wait,
        );
        rec.left_as = Some(
            if seen.present.contains(&pr) {
                left_as::ADMITTED
            } else {
                left_as::GONE
            }
            .to_string(),
        );
        rec.wait_secs = Some(u64::try_from((now - wait.started).num_seconds()).unwrap_or(0));
        out.push(rec);
    }
    state.initialized = true;
    out
}

/// Observe one repo's tick and emit the enter/leave records it implies.
pub fn observe(repo: &str, weighing: &Weighing, claims: &DrawClaims) {
    let seen = seen_from(weighing, claims, &crate::sweep_registry::host_identity());
    let records = {
        let mut state = STATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = state.entry(repo.to_ascii_lowercase()).or_default();
        diff(repo, entry, &seen, Utc::now(), &Provenance::current())
    };
    for record in records {
        super::ops::emit_record(crate::telemetry::TelemetryRecord::MergeLaneWait(record));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn t(min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 21, 12, min, 0).unwrap()
    }

    fn seen(waiting: &[u32], blocker: Option<u32>, present: &[u32]) -> Seen {
        Seen {
            waiting: waiting.iter().map(|p| (*p, "champion_merge")).collect(),
            blocker: blocker.map(|b| (b, true)),
            present: present.iter().copied().collect(),
        }
    }

    fn initialized() -> RepoWaits {
        RepoWaits {
            initialized: true,
            ..RepoWaits::default()
        }
    }

    fn run(state: &mut RepoWaits, s: &Seen, at: DateTime<Utc>) -> Vec<MergeLaneWaitRecord> {
        diff("O/R", state, s, at, &Provenance::current())
    }

    #[test]
    fn a_wait_enters_once_and_leaves_with_its_duration() {
        let mut st = initialized();
        let enter = run(&mut st, &seen(&[7], Some(5), &[]), t(0));
        assert_eq!(enter.len(), 1);
        assert_eq!(enter[0].phase, "enter");
        assert_eq!((enter[0].repo.as_str(), enter[0].pr, enter[0].blocker_pr), ("o/r", 7, 5));
        assert!(!enter[0].wait_started_at_lower_bound);
        // Unchanged next tick: nothing.
        assert!(run(&mut st, &seen(&[7], Some(5), &[]), t(1)).is_empty());
        // The PR is drawable again.
        let leave = run(&mut st, &seen(&[], None, &[7]), t(4));
        assert_eq!(leave.len(), 1);
        assert_eq!(leave[0].phase, "leave");
        assert_eq!(leave[0].left_as.as_deref(), Some("admitted"));
        assert_eq!(leave[0].wait_secs, Some(240));
        assert_eq!(leave[0].wait_started_at, t(0));
        assert!(st.waits.is_empty());
    }

    #[test]
    fn a_pr_that_vanished_left_as_gone() {
        let mut st = initialized();
        run(&mut st, &seen(&[7], Some(5), &[]), t(0));
        let leave = run(&mut st, &seen(&[], None, &[]), t(2));
        assert_eq!(leave[0].left_as.as_deref(), Some("gone"));
    }

    #[test]
    fn a_new_blocker_re_enters_with_the_original_start() {
        let mut st = initialized();
        run(&mut st, &seen(&[7], Some(5), &[]), t(0));
        let again = run(&mut st, &seen(&[7], Some(6), &[]), t(3));
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].phase, "enter");
        assert_eq!(again[0].blocker_pr, 6);
        assert_eq!(again[0].wait_started_at, t(0));
    }

    #[test]
    fn the_first_observation_after_a_restart_is_a_lower_bound() {
        let mut st = RepoWaits::default();
        let enter = run(&mut st, &seen(&[7], Some(5), &[]), t(0));
        assert!(enter[0].wait_started_at_lower_bound);
        // A wait that starts later in the same run is exact.
        let later = run(&mut st, &seen(&[7, 9], Some(5), &[]), t(1));
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].pr, 9);
        assert!(!later[0].wait_started_at_lower_bound);
    }
}
