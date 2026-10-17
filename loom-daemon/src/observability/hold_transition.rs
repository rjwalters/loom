//! A role-runner tick's `hold.transition` facts (#12665): the hold-label
//! writes the tick's agent made, read from the tick's pick journal. No forge
//! calls: the writers journal what they already did.

use chrono::{DateTime, Utc};

use super::pick_journal::JournalEntry;
use crate::telemetry::kinds::hold_transition::{transition, HoldTransitionRecord};
use crate::telemetry::provenance::Provenance;

/// `owner/repo#pr@unix-secs`: names the episode an `add` opens.
#[must_use]
pub fn episode_id(repo: &str, pr: u32, at: DateTime<Utc>) -> String {
    format!("{}#{pr}@{}", repo.to_ascii_lowercase(), at.timestamp())
}

/// One record per journaled hold write, in journal order. Empty for a repo
/// that did not resolve.
#[must_use]
pub fn role_tick_records(
    repo: &str,
    entries: &[JournalEntry],
    now: DateTime<Utc>,
) -> Vec<HoldTransitionRecord> {
    if repo == super::pick_decision::REPO_UNRESOLVED {
        return Vec::new();
    }
    let loom = Provenance::current();
    entries
        .iter()
        .filter_map(|entry| {
            let JournalEntry::Hold {
                at,
                number,
                transition: t,
                reason_code,
                trigger,
                confirmed,
                entered_at,
            } = entry
            else {
                return None;
            };
            let is_add = t == transition::ADD;
            // An `add` opens the episode at the write; a `remove` carries only
            // an entry time its caller knew, and is flagged otherwise.
            let entered_at = if is_add { Some(*at) } else { *entered_at };
            Some(HoldTransitionRecord {
                repo: repo.to_ascii_lowercase(),
                pr: *number,
                transition: t.clone(),
                reason_code: reason_code.clone(),
                trigger: trigger.clone(),
                confirmed: *confirmed,
                episode_id: is_add.then(|| episode_id(repo, *number, *at)),
                entered_at,
                entered_at_lower_bound: entered_at.is_none(),
                event_at: *at,
                observed_at: now,
                loom: loom.clone(),
            })
        })
        .collect()
}

/// Emit [`role_tick_records`] through the global ops sink.
pub fn emit_role_tick(repo: &str, entries: &[JournalEntry]) {
    for record in role_tick_records(repo, entries, Utc::now()) {
        super::ops::emit_record(crate::telemetry::TelemetryRecord::HoldTransition(record));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn hold(minute: u32, t: &str, confirmed: bool, entered: Option<DateTime<Utc>>) -> JournalEntry {
        JournalEntry::Hold {
            at: Utc.with_ymd_and_hms(2026, 10, 17, 12, minute, 0).unwrap(),
            number: 7,
            transition: t.to_string(),
            reason_code: "merge-risk-hold".to_string(),
            trigger: "champion_role".to_string(),
            confirmed,
            entered_at: entered,
        }
    }

    #[test]
    fn an_add_opens_an_episode_at_the_write() {
        let rs = role_tick_records("O/R", &[hold(1, "add", true, None)], Utc::now());
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].repo, "o/r");
        assert_eq!(rs[0].episode_id.as_deref(), Some("o/r#7@1792238460"));
        assert_eq!(rs[0].entered_at, Some(rs[0].event_at));
        assert!(!rs[0].entered_at_lower_bound);
    }

    #[test]
    fn a_remove_with_an_unknown_entry_is_flagged_not_stamped() {
        let rs = role_tick_records("o/r", &[hold(5, "remove", true, None)], Utc::now());
        assert_eq!(rs[0].entered_at, None);
        assert!(rs[0].entered_at_lower_bound);
        assert_eq!(rs[0].episode_id, None);
        let known = Utc.with_ymd_and_hms(2026, 10, 17, 11, 0, 0).unwrap();
        let rs = role_tick_records("o/r", &[hold(5, "remove", true, Some(known))], Utc::now());
        assert_eq!(rs[0].entered_at, Some(known));
        assert!(!rs[0].entered_at_lower_bound);
    }

    #[test]
    fn attempted_and_confirmed_writes_differ() {
        let entries = [hold(1, "add", false, None), hold(2, "add", true, None)];
        let rs = role_tick_records("o/r", &entries, Utc::now());
        assert!(!rs[0].confirmed);
        assert!(rs[1].confirmed);
        assert_ne!(rs[0].episode_id, rs[1].episode_id);
    }

    #[test]
    fn other_entries_and_an_unresolved_repo_emit_nothing() {
        let act = JournalEntry::Act {
            at: Utc::now(),
            number: 7,
            action: "merged".into(),
        };
        assert!(role_tick_records("o/r", &[act], Utc::now()).is_empty());
        let unresolved = super::super::pick_decision::REPO_UNRESOLVED;
        assert!(role_tick_records(unresolved, &[hold(1, "add", true, None)], Utc::now()).is_empty());
    }
}
