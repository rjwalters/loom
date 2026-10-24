//! A Champion merge / hold-reevaluate step's `champion.run` fact (#13029):
//! what the run decided, built at the step's end from what it already wrote
//! (its pick journal and its `pass.verdict`), plus the policy digests. No forge
//! calls. See [`crate::telemetry::kinds::champion_run`].

use chrono::{DateTime, Utc};

use super::pick_journal::{Journal, JournalEntry};
use super::step_verdict::StepSource;
use crate::step_role::StepRole;
use crate::telemetry::kinds::champion_run::{outcome, ChampionRunRecord};
use crate::telemetry::kinds::hold_transition::transition;
use crate::telemetry::kinds::pass::PassVerdictRecord;
use crate::telemetry::provenance::Provenance;

/// The record for one Champion step end, or `None` for any other step or an
/// item that is not a PR.
#[must_use]
pub fn record_for(
    repo: &str,
    step: &StepSource,
    journal: Option<&Journal>,
    verdict: &PassVerdictRecord,
    auth_after: Option<String>,
    now: DateTime<Utc>,
) -> Option<ChampionRunRecord> {
    if step.step != StepRole::ChampionMerge || verdict.artifact != "pr" {
        return None;
    }
    let mine = |n: u32| n == step.item;
    let mut last_hold: Option<(&str, &str, bool)> = None;
    let mut digest_recorded = false;
    for entry in journal.map_or(&[][..], |j| j.entries.as_slice()) {
        match entry {
            JournalEntry::Hold {
                number,
                transition: t,
                reason_code,
                confirmed,
                ..
            } if mine(*number) => last_hold = Some((t, reason_code, *confirmed)),
            JournalEntry::Marker { number, marker, .. }
                if mine(*number)
                    && marker.starts_with("<!-- champion:hold-state")
                    && marker.contains(" auth=") =>
            {
                digest_recorded = true;
            }
            _ => {}
        }
    }
    let reason = verdict.reason.clone().unwrap_or_default();
    let outcome = if reason == "merged" {
        outcome::MERGED
    } else if let Some((t, _, confirmed)) = last_hold {
        if !confirmed {
            outcome::ERROR
        } else if t == transition::REMOVE {
            outcome::RELEASED
        } else {
            outcome::HELD
        }
    } else if verdict.verdict == "failed" {
        outcome::ERROR
    } else if reason == "held" || reason == "critical_file" {
        outcome::HELD
    } else {
        outcome::OTHER
    };
    let hold_class = last_hold
        .map(|(_, class, _)| class.to_string())
        .or_else(|| {
            (reason == "critical_file").then(|| {
                crate::telemetry::kinds::hold_transition::reason::CRITICAL_FILE_HOLD.to_string()
            })
        });
    Some(ChampionRunRecord {
        repo: repo.to_ascii_lowercase(),
        pr: step.item,
        sweep_id: step.sweep_id.clone(),
        outcome: outcome.to_string(),
        hold_class,
        verdict_reason: reason,
        auth_before: step.hold_auth_before.clone(),
        auth_after,
        digest_recorded,
        event_at: now,
        loom: Provenance::current(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 21, 12, 0, 0).unwrap()
    }

    fn step(item: u32, role: StepRole) -> StepSource {
        StepSource {
            sweep_id: format!("step-{}-{item}-1", role.as_str()),
            step: role,
            item,
            log_text: None,
            hold_auth_before: Some("aaaa".into()),
        }
    }

    fn verdict(class: &str, reason: &str, artifact: &str) -> PassVerdictRecord {
        let mut v: PassVerdictRecord = serde_json::from_value(serde_json::json!({
            "pass_id": "p", "mechanism": "role_step", "repo": "o/r", "number": 7,
            "artifact": artifact, "verdict": class, "reason": reason,
            "mode": "on", "applied": true, "at": "2026-10-21T12:00:00Z",
        }))
        .unwrap();
        v.verdict = class.into();
        v
    }

    fn hold(t: &str, confirmed: bool) -> JournalEntry {
        JournalEntry::Hold {
            at: now(),
            number: 7,
            transition: t.into(),
            reason_code: "merge-risk-hold".into(),
            trigger: "champion_role".into(),
            confirmed,
            entered_at: None,
            confidence_pct: None,
        }
    }

    fn journal(entries: Vec<JournalEntry>) -> Journal {
        Journal {
            entries,
            ..Journal::default()
        }
    }

    fn run(v: &PassVerdictRecord, j: Option<&Journal>) -> ChampionRunRecord {
        record_for("O/R", &step(7, StepRole::ChampionMerge), j, v, Some("bbbb".into()), now())
            .unwrap()
    }

    #[test]
    fn a_merge_is_merged_whatever_else_was_written() {
        let r =
            run(&verdict("verdict", "merged", "pr"), Some(&journal(vec![hold("remove", true)])));
        assert_eq!(r.outcome, "merged");
        assert_eq!(r.repo, "o/r");
        assert_eq!(r.auth_before.as_deref(), Some("aaaa"));
        assert_eq!(r.auth_after.as_deref(), Some("bbbb"));
    }

    #[test]
    fn hold_writes_decide_held_released_and_error() {
        let v = verdict("noop", "held", "pr");
        let held = run(&v, Some(&journal(vec![hold("add", true)])));
        assert_eq!(
            (held.outcome.as_str(), held.hold_class.as_deref()),
            ("held", Some("merge-risk-hold"))
        );
        assert_eq!(run(&v, Some(&journal(vec![hold("remove", true)]))).outcome, "released");
        assert_eq!(run(&v, Some(&journal(vec![hold("add", false)]))).outcome, "error");
    }

    #[test]
    fn a_failed_step_is_an_error_and_a_requeue_is_other() {
        assert_eq!(run(&verdict("failed", "failed", "pr"), None).outcome, "error");
        let other = run(&verdict("noop", "ci_pending", "pr"), None);
        assert_eq!(other.outcome, "other");
        assert_eq!(other.verdict_reason, "ci_pending");
        assert_eq!(other.hold_class, None);
    }

    #[test]
    fn a_hold_state_marker_with_a_digest_is_a_recorded_digest() {
        let marker = |m: &str| JournalEntry::Marker {
            at: now(),
            number: 7,
            is_pr: true,
            marker: m.into(),
            head_sha: None,
        };
        let v = verdict("noop", "held", "pr");
        let with = journal(vec![marker("<!-- champion:hold-state head=abc auth=bbbb -->")]);
        assert!(run(&v, Some(&with)).digest_recorded);
        let without = journal(vec![marker("<!-- champion:hold-state head=abc -->")]);
        assert!(!run(&v, Some(&without)).digest_recorded);
    }

    #[test]
    fn other_steps_and_issues_have_no_record() {
        let v = verdict("verdict", "merged", "pr");
        assert!(record_for("o/r", &step(7, StepRole::Judge), None, &v, None, now()).is_none());
        let issue = verdict("verdict", "promoted", "issue");
        assert!(record_for("o/r", &step(7, StepRole::ChampionMerge), None, &issue, None, now())
            .is_none());
    }
}
