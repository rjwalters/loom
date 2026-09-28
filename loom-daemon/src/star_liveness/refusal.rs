//! Detect a forge merge refusal on an approved PR from its comments (the
//! #9268 shape). Pure.
//!
//! `merge-pr.sh` hard-stops on a merge error it cannot retry ("Failed to merge
//! PR #N: …"), and Champion then posts its "**Champion: Merge Failed**"
//! comment quoting that text (`champion-pr-merge.md`). A refusal is such a
//! comment whose quoted error is a **policy** refusal: the forge will refuse
//! every retry until a human changes a repo setting. Transient errors are not
//! refusals: "Merge already in progress" (a 405 too), "Base branch was
//! modified" and "Head branch was modified" are the retry ladder's own routes.
//!
//! The ask never quotes the forge text. [`classify_refusal`] maps it to one of
//! a few fixed descriptions, so nothing free-form reaches `queue.snapshot`.
//!
//! # The incident
//!
//! Whoever files the incident for a repo-wide refusal (#9268 for #8191's PR
//! #9276) usually says so on the PR afterwards ("blocked by #9268"). The last
//! `#N` mentioned in the refusal comment or any later comment, other than the
//! PR and its issue, is taken as the incident. It inherits the star.

use super::forge::ForgeComment;
use super::landing::MergeRefusal;

/// Markers of a failed-merge report.
const FAILURE_MARKERS: &[&str] = &[
    "Champion: Merge Failed",
    "Failed to merge PR #",
    "<!-- loom:merge-refused",
];

/// Transient failures the retry ladder handles; never a refusal.
const TRANSIENT: &[&str] = &[
    "Merge already in progress",
    "Base branch was modified",
    "Head branch was modified",
    "head out of date",
];

/// The refusal class for `text`, or `None` when it is not a policy refusal.
#[must_use]
pub fn classify_refusal(text: &str) -> Option<&'static str> {
    if TRANSIENT.iter().any(|t| text.contains(t)) {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("merge commits are not allowed") {
        return Some("merge commits are not allowed on this repository (HTTP 405)");
    }
    if lower.contains("squash merges are not allowed")
        || lower.contains("rebase merges are not allowed")
    {
        return Some("that merge method is not allowed on this repository (HTTP 405)");
    }
    if lower.contains("allowed method")
        || lower.contains("merge method")
        || lower.contains("merge-method")
    {
        return Some("no merge method both the ruleset and the repo settings allow");
    }
    if lower.contains("repository rule violation") || lower.contains("ruleset") {
        return Some("a branch ruleset refuses the merge");
    }
    if lower.contains("http 405") || lower.contains("405 method not allowed") {
        return Some("the forge refuses the merge (HTTP 405)");
    }
    None
}

/// `#N` references in `text`, in order (bare `#123`; not `owner/repo#123`).
fn hash_refs(text: &str) -> Vec<u32> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'#'
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'/')
        {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > start {
                if let Ok(n) = text[start..end].parse::<u32>() {
                    out.push(n);
                }
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// The refusal on PR `pr` (linked to `issue`), from its comments in forge
/// order. The **latest** failed-merge report decides: a later report that is
/// not a policy refusal (a transient failure after the admin fixed the
/// settings) clears it.
#[must_use]
pub fn detect(comments: &[ForgeComment], pr: u32, issue: u32) -> Option<MergeRefusal> {
    let (idx, reason) = comments
        .iter()
        .enumerate()
        .rev()
        .find(|(_, c)| FAILURE_MARKERS.iter().any(|m| c.body.contains(m)))
        .map(|(i, c)| (i, classify_refusal(&c.body)))?;
    let reason = reason?;
    let incident = comments[idx..]
        .iter()
        .flat_map(|c| hash_refs(&c.body))
        .rfind(|n| *n != pr && *n != issue);
    Some(MergeRefusal { reason, incident })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(body: &str) -> ForgeComment {
        ForgeComment {
            body: body.to_string(),
            created_at: None,
        }
    }

    #[test]
    fn the_9268_shape_is_a_refusal_with_its_incident() {
        let comments = [
            c("Judge: approved"),
            c("**Champion: Merge Failed**\n\n```\nFailed to merge PR #9276: gh: Merge commits are not allowed on this repository. (HTTP 405)\n```"),
            c("Also hit this. Repo-wide: see #9268 (ruleset allows squash only)."),
        ];
        let r = detect(&comments, 9276, 8191).unwrap();
        assert!(r.reason.contains("405"), "{}", r.reason);
        assert_eq!(r.incident, Some(9268));
    }

    #[test]
    fn transient_failures_are_not_refusals() {
        for text in [
            "**Champion: Merge Failed**\nMerge already in progress (HTTP 405)",
            "**Champion: Merge Failed**\nBase branch was modified",
        ] {
            assert_eq!(detect(&[c(text)], 1, 2), None, "{text}");
        }
    }

    #[test]
    fn a_later_non_policy_failure_clears_the_refusal() {
        let comments = [
            c("**Champion: Merge Failed**\nHTTP 405: Merge commits are not allowed"),
            c("**Champion: Merge Failed**\nHead branch was modified"),
        ];
        assert_eq!(detect(&comments, 1, 2), None);
    }

    #[test]
    fn no_failure_report_is_no_refusal_and_self_refs_are_not_incidents() {
        assert_eq!(detect(&[c("see #5")], 1, 2), None);
        let r = detect(
            &[c(
                "Failed to merge PR #7: Repository rule violations found (closes #3)",
            )],
            7,
            3,
        )
        .unwrap();
        assert_eq!(r.incident, None);
        assert_eq!(hash_refs("a#1 x/y#2 #3 (#44)"), vec![3, 44]);
    }
}
