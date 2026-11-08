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
///
/// A record written in the *same* comment as the clear carries the head at
/// Champion's tick, not the head the human cleared (the head may have moved in
/// between). Such a clear covers `head` only when the hold it released was
/// itself recorded at `head`: the previous record must equal `head` too.
#[must_use]
pub fn champion_clear_at<C: AsRef<str>>(trusted: &[C], head: &str) -> bool {
    clear_verdict(trusted, head) == ClearVerdict::Covers
}

#[derive(Debug, PartialEq, Eq)]
enum ClearVerdict {
    Covers,
    /// No record, or no clear after it: nothing ties a clear to any head.
    NoClear,
    /// A record exists but the clear is not provably for the live head.
    HeadMismatch,
}

fn clear_verdict<C: AsRef<str>>(trusted: &[C], head: &str) -> ClearVerdict {
    let Some(rec) = trusted
        .iter()
        .rposition(|c| recorded_head(c.as_ref()).is_some())
    else {
        return ClearVerdict::NoClear;
    };
    if recorded_head(trusted[rec].as_ref()).as_deref() != Some(head) {
        return ClearVerdict::HeadMismatch;
    }
    if standing_hold(trusted).is_some() {
        return ClearVerdict::NoClear;
    }
    if trusted[rec + 1..].iter().any(|c| is_clear(c.as_ref())) {
        return ClearVerdict::Covers;
    }
    if !is_clear(trusted[rec].as_ref()) {
        return ClearVerdict::NoClear;
    }
    // Clear shares the record's comment: the released hold must be at `head`.
    let prior = trusted[..rec]
        .iter()
        .rev()
        .find_map(|c| recorded_head(c.as_ref()));
    if prior.as_deref() == Some(head) {
        ClearVerdict::Covers
    } else {
        ClearVerdict::HeadMismatch
    }
}

/// `None` to proceed; `Some(reason)` to wait.
#[must_use]
pub fn decide(proven: bool, comments: Result<Vec<String>, String>, head: &str) -> Option<String> {
    if proven {
        return None;
    }
    match comments {
        Ok(c) => match clear_verdict(&c, head) {
            ClearVerdict::Covers => None,
            ClearVerdict::NoClear => {
                Some(format!("no byte-identity proof and no Champion clear at {head}"))
            }
            ClearVerdict::HeadMismatch => Some(format!(
                "clear-head-mismatch: Champion clear is not provably for live head {head}"
            )),
        },
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

    fn release(h: &str) -> String {
        format!("{MERGE_RISK_CLEARED_MARKER}\nreleased\n<!-- champion:hold-state head={h} -->")
    }

    #[test]
    fn same_comment_clear_and_record_at_moved_head_waits_with_token() {
        // Hold at A, human clears at A, head pushes to B, Champion writes a
        // release+record at B: the clear never covered B.
        let r = decide(false, Ok(vec![hold(H1), release(H2)]), H2);
        assert!(r.unwrap().contains("clear-head-mismatch"));
    }

    #[test]
    fn same_comment_clear_and_record_after_hold_at_same_head_enqueues() {
        assert!(decide(false, Ok(vec![hold(H1), release(H1)]), H1).is_none());
    }

    #[test]
    fn same_comment_clear_without_prior_record_waits() {
        assert!(decide(false, Ok(vec![release(H1)]), H1).is_some());
    }

    #[test]
    fn older_head_record_reports_mismatch_token() {
        let r = decide(false, Ok(vec![hold(H1), cleared()]), H2);
        assert!(r.unwrap().contains("clear-head-mismatch"));
    }

    #[test]
    fn no_record_keeps_legacy_reason() {
        let r = decide(false, Ok(vec![cleared()]), H1).unwrap();
        assert!(r.contains("no byte-identity proof") && !r.contains("clear-head-mismatch"));
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
