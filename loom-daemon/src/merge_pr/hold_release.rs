//! `merge-pr.sh <N> --release-hold`: the operator's structured release of a
//! Champion human-merge hold (#13517).
//!
//! Champion's merge-risk / critical-file hold leaves a Judge-approved PR with
//! `loom:pr` AND `loom:blocked`. [`super::labels::contradiction`] refuses that
//! shape, by design (#8112). Releasing it used to be a hand
//! `gh pr edit --remove-label loom:blocked`, which records nothing and checks
//! nothing. This module decides whether a release is allowed; it releases
//! only when ALL of these hold, and otherwise writes nothing:
//!
//! 1. labels are exactly the hold shape ([`super::labels::hold_shape`]): no
//!    other contradicting label, no `loom:frozen`;
//! 2. a trusted `champion:hold-state head=<sha>` marker exists and names the
//!    current head;
//! 3. the newest trusted `loom:verdict-sha` marker is an `approved` verdict
//!    on the current head.
//!
//! Every input is forge text, so a missing or unparseable input is a refusal,
//! never a pass. `comments` must already be filtered to trusted authors
//! (#9548) by the caller.

use regex::Regex;

/// First stdout line of an eligible release; the audit-comment body follows.
pub const OK: &str = "LOOM-HOLD-RELEASE-OK";

/// The marker the audit comment carries.
pub const RELEASED_MARKER: &str = "champion:hold-released-by-operator";

/// The outcome of [`evaluate`].
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Release and merge; the string is the audit comment body.
    Release(String),
    /// Refuse, with the reason. Nothing is written.
    Refuse(String),
}

/// The SHA of the newest `loom:verdict-sha` marker and whether it approved.
fn last_verdict(comments: &str) -> Option<(String, bool)> {
    let re = Regex::new(
        r"<!-- loom:verdict-sha sha=([0-9a-f]{7,40}) verdict=(approved|changes-requested) -->",
    )
    .ok()?;
    re.captures_iter(comments)
        .last()
        .map(|c| (c[1].to_string(), &c[2] == "approved"))
}

/// Decide whether `operator` may release the hold on PR `pr` at `head_sha`.
#[must_use]
pub fn evaluate(
    pr: &str,
    labels: &str,
    comments: &str,
    head_sha: &str,
    operator: &str,
) -> Decision {
    let refuse = |why: &str| {
        Decision::Refuse(format!(
            "Merge blocked: --release-hold refused for PR #{pr}: {why} Nothing was written."
        ))
    };
    if operator.trim().is_empty() {
        return refuse("the releasing operator's forge login could not be read.");
    }
    if head_sha.len() < 7 || !head_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return refuse("the current head SHA is unknown.");
    }
    if !super::labels::hold_shape(labels) {
        return refuse(
            "the PR is not in the Champion hold shape (`loom:pr` + `loom:blocked` and no other \
             contradicting label). Any other contradiction is not releasable by this path.",
        );
    }
    match super::hold_state::recorded_head(comments) {
        None => {
            return refuse("no trusted `champion:hold-state` marker records a Champion hold.");
        }
        Some(h) if h != head_sha => {
            return refuse(&format!(
                "the head moved since Champion's hold (hold-state head={h}, current head={head_sha})."
            ));
        }
        Some(_) => {}
    }
    match last_verdict(comments) {
        Some((sha, true)) if crate::verdict_gate::same_sha(&sha, head_sha) => {}
        Some((sha, true)) => {
            return refuse(&format!(
                "the head moved since the Judge approval (approved head={sha}, current \
                 head={head_sha})."
            ));
        }
        _ => {
            return refuse(
                "the newest trusted Judge verdict is not an approval (or none exists) for the \
                 current head.",
            );
        }
    }
    Decision::Release(format!(
        "**Champion hold released by operator** @{operator} released the human-merge hold on \
         this PR and is merging it at approved head `{head_sha}`.\n\n\
         `merge-pr.sh --release-hold` verified: labels were only `loom:pr` + `loom:blocked`, the \
         Champion hold-state head and the Judge-approved head both equal `{head_sha}`.\n\n\
         <!-- {RELEASED_MARKER} head={head_sha} -->"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "abcdef1234567890abcdef1234567890abcdef12";

    fn comments(hold: &str, verdict: &str, v: &str) -> String {
        format!(
            "<!-- champion:hold-state head={hold} -->\n<!-- loom:verdict-sha sha={verdict} verdict={v} -->\n"
        )
    }
    const LBL: &str = "loom:pr\nloom:blocked\n";

    #[test]
    fn releases_at_matching_heads() {
        let d = evaluate("5", LBL, &comments(H, H, "approved"), H, "alice");
        assert!(matches!(&d, Decision::Release(b) if b.contains("@alice") && b.contains(H)));
    }
    #[test]
    fn refuses_moved_hold_head() {
        let d = evaluate("5", LBL, &comments("1234567", H, "approved"), H, "alice");
        assert!(matches!(d, Decision::Refuse(m) if m.contains("since Champion's hold")));
    }
    #[test]
    fn refuses_moved_judge_head() {
        let d = evaluate("5", LBL, &comments(H, "1234567", "approved"), H, "alice");
        assert!(matches!(d, Decision::Refuse(m) if m.contains("Judge approval")));
    }
    #[test]
    fn refuses_rejected_or_missing_verdict() {
        let d = evaluate("5", LBL, &comments(H, H, "changes-requested"), H, "alice");
        assert!(matches!(d, Decision::Refuse(_)));
        let only_hold = format!("<!-- champion:hold-state head={H} -->");
        assert!(matches!(evaluate("5", LBL, &only_hold, H, "alice"), Decision::Refuse(_)));
    }
    #[test]
    fn refuses_missing_marker() {
        let c = format!("<!-- loom:verdict-sha sha={H} verdict=approved -->");
        assert!(matches!(evaluate("5", LBL, &c, H, "alice"), Decision::Refuse(_)));
    }
    #[test]
    fn refuses_extra_contradicting_label() {
        let c = comments(H, H, "approved");
        for extra in [
            "loom:changes-requested",
            "loom:operator",
            "loom:sequenced",
            "loom:frozen",
        ] {
            let l = format!("{LBL}{extra}\n");
            assert!(matches!(evaluate("5", &l, &c, H, "alice"), Decision::Refuse(_)), "{extra}");
        }
        assert!(matches!(evaluate("5", "loom:pr\n", &c, H, "alice"), Decision::Refuse(_)));
        assert!(matches!(evaluate("5", "loom:blocked\n", &c, H, "alice"), Decision::Refuse(_)));
    }
    #[test]
    fn refuses_unknown_operator_or_head() {
        let c = comments(H, H, "approved");
        assert!(matches!(evaluate("5", LBL, &c, H, " "), Decision::Refuse(_)));
        assert!(matches!(evaluate("5", LBL, &c, "", "alice"), Decision::Refuse(_)));
    }
}
