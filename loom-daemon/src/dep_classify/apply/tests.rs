//! Tests for applying an un-escalation (epic #7810, PR 3).
//!
//! Write ORDER is the property under test, not the individual writes. The
//! shell could only reach this through a `gh` stub on `PATH`; a recording
//! writer makes the ordering directly assertable.

use super::*;
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Output};

const OPERATOR_ONLY: &str = "loom:operator-only";
const OPERATOR_BLOCKED: &str = "loom:operator-blocked";

fn labels() -> Labels<'static> {
    Labels {
        operator_only: OPERATOR_ONLY,
        operator_blocked: OPERATOR_BLOCKED,
    }
}

fn ok() -> CmdOutcome {
    CmdOutcome::Ran(Output {
        status: ExitStatus::from_raw(0),
        stdout: Vec::new(),
        stderr: Vec::new(),
    })
}

fn refused(msg: &str) -> CmdOutcome {
    CmdOutcome::Ran(Output {
        status: ExitStatus::from_raw(256), // exit code 1
        stdout: Vec::new(),
        stderr: msg.as_bytes().to_vec(),
    })
}

/// Records every write in order, and fails whichever ones are named.
#[derive(Default)]
struct Recorder {
    calls: Vec<String>,
    fail_label: Option<String>,
    fail_comment: bool,
}

impl Writer for Recorder {
    fn remove_label(&mut self, label: &str) -> CmdOutcome {
        self.calls.push(format!("remove-label {label}"));
        if self.fail_label.as_deref() == Some(label) {
            return refused("simulated label failure");
        }
        ok()
    }
    fn post_comment(&mut self, body: &str) -> CmdOutcome {
        self.calls
            .push(format!("comment {}", body.lines().next().unwrap_or("")));
        if self.fail_comment {
            return refused("simulated comment failure");
        }
        ok()
    }
}

#[test]
fn the_label_is_removed_before_the_comment_is_posted() {
    // THE ordering. See the module docs: comment-first plus a failed removal
    // permanently suppresses every future retry, because the idempotency guard
    // keys on the comment.
    let mut w = Recorder::default();
    apply_unescalation(&mut w, &labels(), "**Champion: Un-escalating**\nbody").unwrap();

    let label_at = w
        .calls
        .iter()
        .position(|c| c.contains(OPERATOR_ONLY))
        .unwrap();
    let comment_at = w
        .calls
        .iter()
        .position(|c| c.starts_with("comment"))
        .unwrap();
    assert!(
        label_at < comment_at,
        "the label must come off before the marker is posted: {:?}",
        w.calls
    );
}

#[test]
fn a_failed_label_removal_posts_no_comment_at_all() {
    // The whole point of the ordering: leave the issue retryable.
    let mut w = Recorder {
        fail_label: Some(OPERATOR_ONLY.to_string()),
        ..Default::default()
    };
    let err = apply_unescalation(&mut w, &labels(), "body").unwrap_err();

    assert!(matches!(err, ApplyError::LabelRemoval(_)), "got {err:?}");
    assert!(
        !w.calls.iter().any(|c| c.starts_with("comment")),
        "no comment may be posted when the label removal failed: {:?}",
        w.calls
    );
    assert!(
        !w.calls.iter().any(|c| c.contains(OPERATOR_BLOCKED)),
        "the sub-label removal must not be attempted either: {:?}",
        w.calls
    );
}

#[test]
fn a_failed_comment_is_reported_but_the_label_is_already_off() {
    // The soft direction: the state change that matters landed. A later re-scan
    // stops at `not-operator-only`; only the audit trail is missing.
    let mut w = Recorder {
        fail_comment: true,
        ..Default::default()
    };
    let err = apply_unescalation(&mut w, &labels(), "body").unwrap_err();

    assert!(matches!(err, ApplyError::CommentPost(_)), "got {err:?}");
    assert!(
        w.calls
            .iter()
            .any(|c| c == &format!("remove-label {OPERATOR_ONLY}")),
        "the label removal must already have happened: {:?}",
        w.calls
    );
}

#[test]
fn the_sub_label_removal_is_best_effort() {
    // #5671: a sub-kind label must not outlive its base label, but a pre-#5679
    // escalation never carried one — "already absent" is the common case, not
    // an error.
    let mut w = Recorder {
        fail_label: Some(OPERATOR_BLOCKED.to_string()),
        ..Default::default()
    };
    assert!(
        apply_unescalation(&mut w, &labels(), "body").is_ok(),
        "a missing sub-label must not fail the apply: {:?}",
        w.calls
    );
    assert!(w.calls.iter().any(|c| c.starts_with("comment")));
}

#[test]
fn the_two_failure_modes_are_distinguishable() {
    // A caller needs to tell "retry later" from "the release landed, the note
    // did not" — they call for different follow-ups.
    let mut a = Recorder {
        fail_label: Some(OPERATOR_ONLY.to_string()),
        ..Default::default()
    };
    let mut b = Recorder {
        fail_comment: true,
        ..Default::default()
    };
    let ea = apply_unescalation(&mut a, &labels(), "body").unwrap_err();
    let eb = apply_unescalation(&mut b, &labels(), "body").unwrap_err();
    assert_ne!(ea, eb);
    assert!(ea.to_string().contains("retry"), "{ea}");
    assert!(eb.to_string().contains("label removed"), "{eb}");
}

// ---------------------------------------------------------------------------
// Comment bodies
// ---------------------------------------------------------------------------

#[test]
fn the_cleared_body_carries_its_marker_and_names_the_blockers() {
    let body = cleared_body(OPERATOR_ONLY, OPERATOR_BLOCKED, "o/r#3", "<!-- m:abc -->");
    assert!(body.contains("the recorded blocker has closed"), "{body}");
    assert!(body.contains("**Cleared blockers**: o/r#3"), "{body}");
    assert!(body.trim_end().ends_with("<!-- m:abc -->"), "the marker must be last: {body}");
}

#[test]
fn the_subset_body_carries_its_marker_and_the_subset_text() {
    let body = subset_body(
        OPERATOR_ONLY,
        OPERATOR_BLOCKED,
        "o/r#3",
        "- the independent half",
        "<!-- m:subset-abc -->",
    );
    assert!(body.contains("a startable subset was never actually blocked"), "{body}");
    assert!(body.contains("- the independent half"), "{body}");
    assert!(body.contains("scoped to the"), "{body}");
    assert!(body.trim_end().ends_with("<!-- m:subset-abc -->"), "{body}");
}

#[test]
fn both_bodies_say_a_human_can_re_park_the_proposal() {
    // The escape hatch matters: this mechanism must never read as overriding a
    // human decision, and the comment is where an operator learns that.
    for body in [
        cleared_body(OPERATOR_ONLY, OPERATOR_BLOCKED, "o/r#3", "<!-- m -->"),
        subset_body(OPERATOR_ONLY, OPERATOR_BLOCKED, "o/r#3", "- w", "<!-- m -->"),
    ] {
        assert!(
            body.contains("re-add") && body.contains("not be un-escalated again"),
            "the body must tell an operator how to re-park it: {body}"
        );
    }
}
