//! The daemon-landing gate (r7-q35): a resync PR is handed to the merge queue
//! only on the byte-identity proof or a Champion clear covering the live head.
//! A Judge verdict alone is not enough. Fail closed: any read failure waits.

use super::forge::LifecycleForge;
use crate::champion_hold::{
    standing_hold, CRITICAL_FILE_CLEARED_MARKER, CRITICAL_FILE_RELEASED_MARKER,
    MERGE_RISK_CLEARED_MARKER,
};
use crate::merge_pr::hold_state::recorded_head;

/// Markers that end a Champion hold episode.
fn is_clear(body: &str) -> bool {
    body.contains(MERGE_RISK_CLEARED_MARKER)
        || body.starts_with(CRITICAL_FILE_CLEARED_MARKER)
        || body.starts_with(CRITICAL_FILE_RELEASED_MARKER)
        || body.starts_with(crate::champion_hold::ratification::CLEARED_MARKER)
        || body.starts_with(crate::champion_hold::ratification::RELEASED_MARKER)
}

/// True when the newest `champion:hold-state` record is for `head`, a clear
/// or release marker follows it (or shares its comment), and no hold stands.
/// `trusted` is the PR's trusted comment bodies, oldest first (an untrusted
/// quote of a marker is prose, not state). A clear is head-bound only through
/// the hold-state record it follows: with no record at `head` it is not one.
#[must_use]
pub fn champion_clear_at<C: AsRef<str>>(trusted: &[C], head: &str) -> bool {
    let Some(rec) = trusted
        .iter()
        .rposition(|c| recorded_head(c.as_ref()).is_some())
    else {
        return false;
    };
    if recorded_head(trusted[rec].as_ref()).as_deref() != Some(head) {
        return false;
    }
    trusted[rec..].iter().any(|c| is_clear(c.as_ref())) && standing_hold(trusted).is_none()
}

/// `None` to proceed; `Some(reason)` to wait.
#[must_use]
pub fn decide(proven: bool, comments: Result<Vec<String>, String>, head: &str) -> Option<String> {
    if proven {
        return None;
    }
    match comments {
        Ok(c) if champion_clear_at(&c, head) => None,
        Ok(_) => Some(format!("no byte-identity proof and no Champion clear at {head}")),
        Err(e) => Some(format!("Champion clear unreadable at {head} ({e}); not enqueued")),
    }
}

/// [`decide`] over the live forge.
#[must_use]
pub fn gate(forge: &dyn LifecycleForge, pr: u32, head: &str, proven: bool) -> Option<String> {
    if proven {
        return None;
    }
    decide(false, forge.trusted_comments(pr), head)
}

#[cfg(test)]
mod tests {
    use super::*;

    const H1: &str = "aaaa1111";
    const H2: &str = "bbbb2222";

    fn hold(h: &str) -> String {
        format!("<!-- champion:merge-risk-hold -->\nheld\n<!-- champion:hold-state head={h} -->")
    }
    fn cleared() -> String {
        format!("{MERGE_RISK_CLEARED_MARKER}\n**Reversing a prior merge-risk hold**")
    }
    fn verdict() -> String {
        "<!-- loom:verdict-sha=aaaa1111 -->\napproved".to_string()
    }

    #[test]
    fn verdict_only_waits() {
        assert!(decide(false, Ok(vec![verdict()]), H1).is_some());
    }

    #[test]
    fn proof_enqueues_without_reading_comments() {
        assert!(decide(true, Err("boom".into()), H1).is_none());
    }

    #[test]
    fn clear_at_live_head_enqueues() {
        assert!(decide(false, Ok(vec![verdict(), hold(H1), cleared()]), H1).is_none());
    }

    #[test]
    fn clear_at_older_head_waits() {
        assert!(decide(false, Ok(vec![hold(H1), cleared()]), H2).is_some());
    }

    #[test]
    fn newer_hold_after_clear_waits() {
        assert!(decide(false, Ok(vec![hold(H1), cleared(), hold(H2)]), H2).is_some());
    }

    #[test]
    fn read_failure_waits() {
        assert!(decide(false, Err("rate limited".into()), H1).is_some());
    }

    #[test]
    fn empty_comments_wait() {
        assert!(decide(false, Ok(vec![]), H1).is_some());
    }

    #[test]
    fn loom_ui_6374_and_6518_timeline_waits() {
        // Judge verdict, then a merge-risk hold on a merge-gate file, no clear.
        for h in [H1, H2] {
            assert!(decide(false, Ok(vec![verdict(), hold(h)]), h).is_some());
        }
    }
}
