//! A role-runner tick's `stage.work_started` facts (Issue #11641): the first
//! forge write a Judge, Doctor or Champion agent issued on each PR, read from
//! the tick's pick journal. No forge calls: the journal is the one the tick's
//! `pick.decision` already reads.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use super::pick_journal::JournalEntry;
use crate::telemetry::kinds::work_started::{
    role, role_tick_stage, source, WorkStartedRecord, ROLE_RUNNER_ORIGIN,
};
use crate::telemetry::provenance::Provenance;

/// The journal actions that mean `role_name` is working the PR it names. A
/// Champion's other writes (promoting issues, evaluating proposals) are not PR
/// work, so only its merge counts.
fn starts_work(role_name: &str, action: &str) -> bool {
    match role_name {
        role::JUDGE => matches!(action, "claimed" | "approved" | "changes_requested"),
        role::DOCTOR => matches!(action, "claimed" | "labeled"),
        role::CHAMPION => action == "merged",
        _ => false,
    }
}

/// One record per PR the tick's agent worked, dated by its earliest qualifying
/// write. Empty for a role whose start this kind does not record, and for a
/// repo that did not resolve.
#[must_use]
pub fn role_tick_records(
    role_name: &str,
    repo: &str,
    tick_id: &str,
    entries: &[JournalEntry],
    now: DateTime<Utc>,
) -> Vec<WorkStartedRecord> {
    let Some(stage) = role_tick_stage(role_name) else {
        return Vec::new();
    };
    if repo == super::pick_decision::REPO_UNRESOLVED {
        return Vec::new();
    }
    let mut first: BTreeMap<u32, DateTime<Utc>> = BTreeMap::new();
    for entry in entries {
        if let JournalEntry::Act { at, number, action } = entry {
            if starts_work(role_name, action) {
                first
                    .entry(*number)
                    .and_modify(|t| *t = (*t).min(*at))
                    .or_insert(*at);
            }
        }
    }
    let loom = Provenance::current();
    first
        .into_iter()
        .map(|(number, started_at)| WorkStartedRecord {
            repo: repo.to_ascii_lowercase(),
            issue: None,
            pr_number: Some(number),
            stage,
            role: role_name.to_string(),
            sweep_id: tick_id.to_string(),
            attempt_index: None,
            stage_attempt: 1,
            dispatch_origin: Some(ROLE_RUNNER_ORIGIN.to_string()),
            started_at,
            source: source::ROLE_ACT.to_string(),
            observed_at: now,
            loom: loom.clone(),
        })
        .collect()
}

/// Emit [`role_tick_records`] through the global ops sink.
pub fn emit_role_tick(role_name: &str, repo: &str, tick_id: &str, entries: &[JournalEntry]) {
    for record in role_tick_records(role_name, repo, tick_id, entries, Utc::now()) {
        super::ops::emit_record(crate::telemetry::TelemetryRecord::WorkStarted(record));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::telemetry::kinds::fleet_state::FleetStage;

    fn act(minute: u32, number: u32, action: &str) -> JournalEntry {
        JournalEntry::Act {
            at: Utc.with_ymd_and_hms(2026, 10, 12, 15, minute, 0).unwrap(),
            number,
            action: action.to_string(),
        }
    }

    #[test]
    fn judge_start_is_its_first_write_per_pr() {
        let entries = [
            JournalEntry::Queue {
                at: Utc::now(),
                role: "judge".into(),
                acts_observable: true,
                total: 0,
                rows: Vec::new(),
            },
            act(5, 101, "claimed"),
            act(9, 101, "approved"),
            act(7, 102, "changes_requested"),
        ];
        let now = Utc::now();
        let records = role_tick_records("judge", "RJWalters/Loom", "role-judge-1", &entries, now);
        assert_eq!(records.len(), 2);
        let r = &records[0];
        assert_eq!(r.pr_number, Some(101));
        assert_eq!(r.issue, None);
        assert_eq!(r.repo, "rjwalters/loom");
        assert_eq!(r.stage, FleetStage::ReviewWait);
        assert_eq!(r.role, "judge");
        assert_eq!(r.sweep_id, "role-judge-1");
        assert_eq!(r.dispatch_origin.as_deref(), Some("role_runner"));
        assert_eq!(r.started_at, Utc.with_ymd_and_hms(2026, 10, 12, 15, 5, 0).unwrap());
        assert_eq!(r.source, "role_act");
        assert_eq!(r.observed_at, now);
        assert_eq!(records[1].pr_number, Some(102));
    }

    #[test]
    fn doctor_and_champion_map_to_their_stages() {
        let doctor = role_tick_records("doctor", "o/r", "t", &[act(1, 7, "claimed")], Utc::now());
        assert_eq!(doctor[0].stage, FleetStage::Doctor);
        let entries = [act(1, 8, "promoted"), act(2, 9, "merged")];
        let champion = role_tick_records("champion", "o/r", "t", &entries, Utc::now());
        assert_eq!(champion.len(), 1, "an issue promotion is not PR work");
        assert_eq!(champion[0].pr_number, Some(9));
        assert_eq!(champion[0].stage, FleetStage::MergeWait);
    }

    #[test]
    fn other_roles_and_unresolved_repos_emit_nothing() {
        let entries = [act(1, 7, "claimed")];
        assert!(role_tick_records("curator", "o/r", "t", &entries, Utc::now()).is_empty());
        let unresolved = super::super::pick_decision::REPO_UNRESOLVED;
        assert!(role_tick_records("judge", unresolved, "t", &entries, Utc::now()).is_empty());
    }
}
