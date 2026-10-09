//! The live status projection of an active drain (Issue #8514), and of the
//! pause-and-roll H4 pause behind an automatic roll (Issue #10831).
//!
//! This module was renamed by #10831. Under its old name it also held #6007's wait-for-zero
//! roll policy (re-arm a refused roll deadline on a widening window, then
//! abandon it once a paused-dispatch budget is spent). #10831 removed that
//! policy: every automatic roll now pauses its agents at a safe point and
//! restarts (`crate::auto_update::pause_roll`, design
//! `docs/design/daemon-roll-pause-resume.md`), so no roll waits for the
//! in-flight count to reach zero any more. What is left is what operator
//! drains and `loom-daemon status` still need:
//!
//! - [`DrainRollStatus`] / [`roll_status`]: "dispatch paused since T for D, N
//!   in flight", answerable from any single `status --json`. The wire shape is
//!   unchanged so older readers still parse it. `roll_pending` is always
//!   `false` and `refusals` always `0` now that a roll is never retained;
//!   `budget_secs` is the pause budget of a `pause-roll` drain and `0`
//!   otherwise.
//! - [`paused_secs`] and [`MAX_DRAIN_PENDING_BUDGET_SECS`], used by the #8652
//!   paused-time ledger.

use super::DrainDescriptor;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The longest open interval the #8652 paused-time ledger
/// ([`super::drain_ledger`]) will count. A killed daemon can leave an interval
/// open; on reload it is closed at no more than this past its start, so one
/// lost process can never book days of "paused" time. (Before #10831 the same
/// number also capped a retained roll's paused-dispatch budget.)
pub const MAX_DRAIN_PENDING_BUDGET_SECS: u64 = 4 * 3600;

/// Progress of the H4 pause behind a `pause-roll` drain (#10831, design §7).
///
/// Carried on [`DrainDescriptor::pause`] while the pause runs and projected
/// into [`DrainRollStatus::pause`], so an operator can see which step the
/// pause is on, whether it can still be aborted, its budget, and what it has
/// done to each agent so far.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseRollStatus {
    /// The H4 step in progress (1-10, design §7). `0` before step 1.
    pub step: u8,
    /// `true` once any agent's process tree has been stopped. From then on an
    /// operator `--abort-drain` is refused and a relaunch request no longer
    /// promotes the drain: work has already been stopped, so the roll
    /// completes.
    pub stopped: bool,
    /// `pauseBudgetSecs` in force for this pause.
    pub budget_secs: u64,
    /// `minResumableAgeSecs` in force for this pause.
    #[serde(default)]
    pub min_resumable_age_secs: u64,
    /// The manifest this pause writes, once step 3 has named it.
    #[serde(default)]
    pub manifest_id: Option<String>,
    /// What triggered the roll: `floor`, `repo_ahead`, `config_restart` or
    /// `autoupdate`.
    #[serde(default)]
    pub target_source: Option<String>,
    /// The version being rolled to, when known.
    #[serde(default)]
    pub to_version: Option<String>,
    /// Agents in the manifest.
    #[serde(default)]
    pub items: u32,
    /// Agents stopped at a safe point (`status = paused`).
    #[serde(default)]
    pub paused: u32,
    /// Agents that exited on their own during the pause.
    #[serde(default)]
    pub exited: u32,
    /// Requeued agents, counted by reason (`young-agent-reset`,
    /// `pause-budget-missed`, `session-not-resumable`, …).
    #[serde(default)]
    pub requeued_by_reason: BTreeMap<String, u32>,
    /// Requeue forge writes deferred to the next start (left `planned`).
    #[serde(default)]
    pub deferred_forge_writes: u32,
    /// Observed seconds spent waiting for `Pending` dispatches to settle
    /// (step 1).
    #[serde(default)]
    pub settle_secs: Option<u64>,
    /// Observed seconds from the first pause request to the last item
    /// resolved (steps 5-7).
    #[serde(default)]
    pub stop_secs: Option<u64>,
}

/// The live, always-queryable view of an active drain (Issue #8514).
///
/// `None` on the wire (or from a pre-#8514 daemon) means "no drain is active";
/// every field describes the **current** drain only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainRollStatus {
    /// Always `false` since #10831: a roll is never retained across a deadline
    /// any more. Kept so older readers still parse the object.
    pub roll_pending: bool,
    /// When the active drain began — the anchor for [`Self::paused_secs`].
    pub started_at: Option<DateTime<Utc>>,
    /// How long dispatch has been paused for this drain, in seconds.
    pub paused_secs: u64,
    /// The pause budget (`pauseBudgetSecs`) of a `pause-roll` drain; `0` for
    /// any other drain (#10831).
    pub budget_secs: u64,
    /// Always `0` since #10831 (no deadline refusals are counted any more).
    pub refusals: u32,
    /// In-flight sweep count at the moment this status was built.
    pub in_flight: usize,
    /// The artifact identity a `pause-roll` drain rolls to (#8514's supersede
    /// key), or `None` for any other drain.
    pub target: Option<String>,
    /// `true` when this drain's terminal action is "exit and stay down"
    /// (a `fleet drain` teardown) rather than "exit for a supervised
    /// relaunch".
    pub then_exit: bool,
    /// Who started (or last promoted) this drain: `"operator"` or
    /// `"pause-roll"` (`"auto-update"` from a pre-#10831 daemon).
    #[serde(default)]
    pub origin: String,
    /// `true` once an operator drain passed its deadline without force and is
    /// being held with dispatch PAUSED (#9588) — it never resumes on its own.
    #[serde(default)]
    pub timed_out: bool,
    /// `true` when dispatch is held because this daemon started while an
    /// operator-stop record existed (#9588).
    #[serde(default)]
    pub startup_hold: bool,
    /// The H4 pause's progress, for a `pause-roll` drain (#10831). `None` for
    /// any other drain, and from a pre-#10831 daemon.
    #[serde(default)]
    pub pause: Option<PauseRollStatus>,
}

/// Project the live drain state into [`DrainRollStatus`] (Issue #8514).
///
/// `None` when no drain is active: the historical `drain_note` already explains
/// a drain that ended, and reporting stale elapsed-pause numbers for a finished
/// drain would be worse than reporting nothing.
#[must_use]
pub fn roll_status(
    snap: &DrainDescriptor,
    in_flight: usize,
    now: DateTime<Utc>,
) -> Option<DrainRollStatus> {
    if !snap.active {
        return None;
    }
    Some(DrainRollStatus {
        roll_pending: false,
        started_at: snap.started_at,
        paused_secs: paused_secs(snap.started_at, now),
        budget_secs: snap.pause.as_ref().map_or(0, |p| p.budget_secs),
        refusals: 0,
        in_flight,
        target: snap.roll_target.clone(),
        then_exit: snap.then_exit,
        origin: snap.origin.as_str().to_string(),
        timed_out: snap.timed_out,
        startup_hold: snap.startup_hold,
        pause: snap.pause.clone(),
    })
}

/// Seconds of paused dispatch so far. Saturating: a `started_at` in the future
/// (a clock step) reads as `0` rather than panicking or wrapping.
#[must_use]
pub fn paused_secs(started_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> u64 {
    started_at.map_or(0, |started| {
        let secs = (now - started).num_seconds();
        u64::try_from(secs).unwrap_or(0)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn descriptor() -> DrainDescriptor {
        DrainDescriptor {
            active: true,
            started_at: Some(Utc::now()),
            ..DrainDescriptor::default()
        }
    }

    // ---- #8514's live projection ------------------------------------------

    #[test]
    fn no_active_drain_projects_to_none() {
        let snap = DrainDescriptor::default();
        assert!(roll_status(&snap, 3, Utc::now()).is_none());
    }

    #[test]
    fn an_active_pause_roll_projects_live_paused_seconds_and_its_pause_budget() {
        let now = Utc::now();
        let mut snap = descriptor();
        snap.origin = super::super::DrainOrigin::PauseRoll;
        snap.started_at = Some(now - chrono::Duration::seconds(90));
        snap.roll_target = Some("v0.19.24".to_string());
        snap.pause = Some(PauseRollStatus {
            step: 5,
            budget_secs: 120,
            ..PauseRollStatus::default()
        });

        let status = roll_status(&snap, 3, now).unwrap();
        assert!(!status.roll_pending, "a roll is never retained any more");
        assert_eq!(status.refusals, 0);
        assert_eq!(status.paused_secs, 90);
        assert_eq!(status.budget_secs, 120, "the pause budget");
        assert_eq!(status.in_flight, 3);
        assert_eq!(status.target.as_deref(), Some("v0.19.24"));
        assert_eq!(status.origin, "pause-roll");
        assert_eq!(status.pause.unwrap().step, 5);
        assert!(!status.then_exit);
    }

    #[test]
    fn an_operator_drain_reports_no_budget_and_no_pause() {
        let status = roll_status(&descriptor(), 1, Utc::now()).unwrap();
        assert_eq!(status.budget_secs, 0);
        assert!(status.pause.is_none());
        assert_eq!(status.origin, "operator");
    }

    #[test]
    fn paused_seconds_grow_between_polls() {
        let start = Utc::now();
        let mut snap = descriptor();
        snap.started_at = Some(start);
        let first = roll_status(&snap, 1, start + chrono::Duration::seconds(30))
            .unwrap()
            .paused_secs;
        let second = roll_status(&snap, 1, start + chrono::Duration::seconds(90))
            .unwrap()
            .paused_secs;
        assert_eq!((first, second), (30, 90));
    }

    #[test]
    fn a_backwards_clock_step_reads_as_zero_not_a_wrapped_duration() {
        let now = Utc::now();
        assert_eq!(paused_secs(Some(now + chrono::Duration::seconds(60)), now), 0);
        assert_eq!(paused_secs(None, now), 0);
    }

    #[test]
    fn a_pre_10831_object_without_a_pause_field_still_parses() {
        let raw = r#"{"roll_pending":true,"started_at":null,"paused_secs":5,"budget_secs":7200,
            "refusals":2,"in_flight":1,"target":null,"then_exit":false,"origin":"auto-update"}"#;
        let status: DrainRollStatus = serde_json::from_str(raw).unwrap();
        assert!(status.pause.is_none());
        assert_eq!(status.origin, "auto-update");
    }

    /// The other direction: a reader built before #10831 (this struct is its
    /// `DrainRollStatus`, field for field) still parses what a pause roll
    /// reports, with `roll_pending = false` and `refusals = 0`.
    #[test]
    fn a_pre_10831_reader_still_parses_a_pause_rolls_status() {
        #[derive(Deserialize)]
        struct OldDrainRollStatus {
            roll_pending: bool,
            started_at: Option<DateTime<Utc>>,
            paused_secs: u64,
            budget_secs: u64,
            refusals: u32,
            in_flight: usize,
            target: Option<String>,
            then_exit: bool,
            #[serde(default)]
            origin: String,
            #[serde(default)]
            timed_out: bool,
            #[serde(default)]
            startup_hold: bool,
        }
        let mut snap = descriptor();
        snap.origin = super::super::DrainOrigin::PauseRoll;
        snap.pause = Some(PauseRollStatus {
            step: 7,
            stopped: true,
            budget_secs: 120,
            requeued_by_reason: [("pause-budget-missed".to_string(), 2)]
                .into_iter()
                .collect(),
            ..PauseRollStatus::default()
        });
        let wire = serde_json::to_string(&roll_status(&snap, 2, Utc::now()).unwrap()).unwrap();
        let old: OldDrainRollStatus = serde_json::from_str(&wire).unwrap();
        assert!(!old.roll_pending);
        assert_eq!((old.refusals, old.budget_secs, old.in_flight), (0, 120, 2));
        assert_eq!(old.origin, "pause-roll");
        assert!(old.started_at.is_some() && old.target.is_none());
        assert!(!old.then_exit && !old.timed_out && !old.startup_hold);
        assert!(old.paused_secs < 5);
    }

    // ---- #8514's roll target, on the real DrainState -----------------------

    use super::super::DrainState;
    use std::time::Duration;

    #[test]
    fn a_relaunch_roll_can_be_labelled_with_the_artifact_it_rolls_to() {
        let drain = DrainState::new();
        drain.begin_as(
            Duration::from_secs(1800),
            false,
            false,
            super::super::DrainOrigin::PauseRoll,
        );
        assert_eq!(drain.snapshot().roll_target, None, "unlabelled until told");
        drain.set_roll_target(Some("v0.19.30@bbbb".to_string()));
        assert_eq!(drain.snapshot().roll_target.as_deref(), Some("v0.19.30@bbbb"));
    }

    #[test]
    fn a_teardown_drain_is_never_labelled_as_a_supersedable_roll() {
        let drain = DrainState::new();
        // `then_exit` — `fleet drain`'s teardown.
        drain.begin(Duration::from_secs(1800), false, true);
        drain.set_roll_target(Some("v0.19.30@bbbb".to_string()));
        assert_eq!(drain.snapshot().roll_target, None);
    }

    #[test]
    fn labelling_when_no_drain_is_active_is_a_no_op() {
        let drain = DrainState::new();
        drain.set_roll_target(Some("v0.19.30@bbbb".to_string()));
        assert_eq!(drain.snapshot().roll_target, None);
    }

    #[test]
    fn aborting_clears_the_target_with_the_rest_of_the_pending_bookkeeping() {
        let drain = DrainState::new();
        drain.begin_as(
            Duration::from_secs(1800),
            false,
            false,
            super::super::DrainOrigin::PauseRoll,
        );
        drain.set_roll_target(Some("v0.19.30@bbbb".to_string()));
        assert!(drain.abort());
        let snap = drain.snapshot();
        assert_eq!(snap.roll_target, None);
        assert!(snap.pause.is_none());
        assert!(!snap.active);
        assert!(!drain.is_draining(), "dispatch resumes");
    }

    #[test]
    fn a_fresh_drain_never_inherits_the_previous_rolls_target() {
        let drain = DrainState::new();
        drain.begin_as(
            Duration::from_secs(1800),
            false,
            false,
            super::super::DrainOrigin::PauseRoll,
        );
        drain.set_roll_target(Some("v0.19.24@aaaa".to_string()));
        drain.abort();
        drain.begin_as(
            Duration::from_secs(1800),
            false,
            false,
            super::super::DrainOrigin::PauseRoll,
        );
        assert_eq!(drain.snapshot().roll_target, None);
    }
}
