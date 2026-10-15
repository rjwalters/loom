//! Judge's projection (#12029): the verdict a Judge wrote on a PR
//! (`judge_verdict_v1`), a closed, versioned reason set.
//!
//! Evidence, strongest first: the `loom:verdict-sha` marker `forge
//! verdict-labels --sha` journals once the verdict's label transition has
//! verified (`pick_journal::record_verdict`; its `head_sha` is the head the
//! verdict was written against), then the step's own `LOOM_STEP_OUTCOME` line
//! (`class:reason`). A role-runner tick has no outcome line, so it projects
//! only the PRs it wrote a verdict on; its no-op signal is `role_tick.outcome`'s
//! `found_work` / `noop_reason`.

use super::{Event, Evidence, Projection, Projector};
use crate::observability::pick_journal::VERDICT_MARKER_KIND;
use crate::step_dispatch::outcome_ledger;
use crate::step_role::StepRole;

/// The Judge verdict's reason set.
pub const REASON_SET: &str = "judge_verdict_v1";

/// Every reason of [`REASON_SET`].
pub const REASONS: [&str; 6] = [
    "approved",
    "changes_requested",
    "already_judged",
    "not_eligible",
    "failed",
    "unclassified",
];

pub(super) const PROJECTOR: Projector = Projector {
    step_name,
    project,
    outcome_word,
};

fn step_name(_is_pr: bool) -> &'static str {
    "judge_pr"
}

/// The step's outcome as `class:reason`, from its `LOOM_STEP_OUTCOME` line.
fn outcome_word(text: &str, _is_pr: bool, item: u32) -> Option<String> {
    let line = outcome_ledger::parse_outcome_line(text, StepRole::Judge, item)?;
    Some(format!("{}:{}", line.class.as_str(), line.reason))
}

fn projection(class: &'static str, reason: &str) -> Projection {
    Projection {
        class,
        reason: reason.to_string(),
        reason_set: REASON_SET,
        ..Projection::default()
    }
}

/// `verdict=<v>` of a `loom:verdict-sha` marker, in the reason set's
/// snake_case.
fn marker_verdict(marker: &str) -> Option<&'static str> {
    let inner = marker.strip_prefix("<!-- ")?;
    let mut words = inner.split_whitespace();
    (words.next()? == VERDICT_MARKER_KIND).then_some(())?;
    words.find_map(|w| match w.strip_prefix("verdict=")? {
        "approved" => Some("approved"),
        "changes-requested" => Some("changes_requested"),
        _ => None,
    })
}

fn known(reason: &str, fallback: &'static str) -> String {
    if REASONS.contains(&reason) {
        reason.to_string()
    } else {
        fallback.to_string()
    }
}

fn project(e: &Evidence) -> Option<Projection> {
    for event in e.events.iter().rev() {
        let Event::Marker {
            at,
            marker,
            head_sha,
        } = event
        else {
            continue;
        };
        if let Some(reason) = marker_verdict(marker) {
            return Some(Projection {
                head_sha: head_sha.clone(),
                wrote: true,
                at: Some(*at),
                ..projection("verdict", reason)
            });
        }
    }
    let Some(word) = e.outcome.as_deref() else {
        return e.step_path.then(|| projection("failed", "unclassified"));
    };
    let (class, reason) = word.split_once(':').unwrap_or((word, ""));
    let p = match class {
        "noop" => projection("noop", &known(reason, "unclassified")),
        "refused" => projection("refused", &known(reason, "unclassified")),
        "failed" => projection("failed", "failed"),
        // A `verdict` line with no verdict write observed: the claim is
        // unconfirmed, so it is not projected as one.
        _ => projection("failed", "unclassified"),
    };
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn marker(verdict: &str, head: &str) -> Event {
        Event::Marker {
            at: Utc::now(),
            marker: format!("<!-- loom:verdict-sha sha={head} verdict={verdict} -->"),
            head_sha: Some(head.to_string()),
        }
    }

    fn evidence(events: Vec<Event>, outcome: Option<&str>, step_path: bool) -> Evidence {
        Evidence {
            number: 7,
            is_pr: true,
            events,
            outcome: outcome.map(str::to_string),
            step_path,
            pre_labels: Vec::new(),
        }
    }

    #[test]
    fn a_verdict_marker_is_the_verdict_at_its_head() {
        let p =
            project(&evidence(vec![marker("changes-requested", "abc1234")], None, false)).unwrap();
        assert_eq!((p.class, p.reason.as_str()), ("verdict", "changes_requested"));
        assert_eq!(p.head_sha.as_deref(), Some("abc1234"));
        assert!(p.wrote);
        let p =
            project(&evidence(vec![marker("approved", "def5678")], Some("noop:x"), true)).unwrap();
        assert_eq!(p.reason, "approved", "a written verdict outranks the outcome line");
    }

    #[test]
    fn a_step_without_a_write_projects_its_outcome_line() {
        let noop = project(&evidence(Vec::new(), Some("noop:already_judged"), true)).unwrap();
        assert_eq!((noop.class, noop.reason.as_str()), ("noop", "already_judged"));
        let odd = project(&evidence(Vec::new(), Some("noop:whatever"), true)).unwrap();
        assert_eq!(odd.reason, "unclassified", "the reason set is closed");
        let failed = project(&evidence(Vec::new(), Some("failed:x"), true)).unwrap();
        assert_eq!((failed.class, failed.reason.as_str()), ("failed", "failed"));
        let none = project(&evidence(Vec::new(), None, true)).unwrap();
        assert_eq!((none.class, none.reason.as_str()), ("failed", "unclassified"));
    }

    #[test]
    fn a_tick_that_wrote_nothing_projects_nothing() {
        assert!(project(&evidence(Vec::new(), None, false)).is_none());
    }

    #[test]
    fn only_the_verdict_marker_is_a_verdict() {
        assert_eq!(marker_verdict("<!-- champion:revise body-ab -->"), None);
        assert_eq!(
            marker_verdict("<!-- loom:verdict-sha sha=abc1234 verdict=approved -->"),
            Some("approved")
        );
    }
}
