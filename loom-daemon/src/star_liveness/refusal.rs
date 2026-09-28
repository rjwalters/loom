//! Detect a forge merge refusal on an approved PR from its comments (the
//! #9268 shape), and the incident issue tied to it. Pure; the forge reads
//! the incident lookup needs happen in [`super::collect`].
//!
//! `merge-pr.sh` hard-stops on a merge error it cannot retry ("Failed to merge
//! PR #N: …"), and Champion (or the sweep) then posts a comment quoting that
//! text. A refusal is such a comment whose quoted error is a **policy**
//! refusal: the forge will refuse every retry until a human changes a repo
//! setting. Transient errors are not refusals: "Merge already in progress"
//! (a 405 too), "Base branch was modified" and "Head branch was modified" are
//! the retry ladder's own routes. Only comments [`super::trust`] believes are
//! read at all.
//!
//! # The incident
//!
//! The incident is tied to the refusal itself, never to whatever a later
//! comment happens to mention (on 2026-09-28 the later comments on PR #9276
//! were a stale-check re-date and a verdict-stale notice naming #8508, #8248
//! and #5686, none of them the incident):
//!
//! 1. an issue named **in the refusal comment** ([`Detected::named`]), if it
//!    is an open issue (not a PR, not closed); else
//! 2. an **open** issue, filed by a **trusted** author ([`super::trust`]),
//!    whose title or body quotes the forge's own refusal phrase
//!    ([`Detected::signature`]), the way #9268 quotes the 405 it was filed
//!    for. Only the three specific phrases in [`SIGNATURES`] are searched,
//!    and a hit must carry the phrase word-bounded ([`quotes_phrase`]). A
//!    generic refusal (a bare 405, "merge method", a ruleset violation) has
//!    no signature: those words are too common to name an incident (`405`
//!    alone matches `#4050`), so it goes straight to the ask.
//!
//! With neither, nothing inherits, and the operator ask carries the raw
//! refusal text ([`Detected::raw`]) so the escalation is never silent.

use super::forge::ForgeComment;

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

/// Longest raw refusal excerpt carried into an ask.
pub const MAX_RAW: usize = 300;

/// The forge refusal phrases an incident issue is searched for: the only
/// classes whose text is specific enough to name one.
pub const SIGNATURES: [&str; 3] = [
    "merge commits are not allowed",
    "squash merges are not allowed",
    "rebase merges are not allowed",
];

/// A refusal class: a fixed description, and the forge phrase an incident
/// issue filed for it would quote (`None` for a generic refusal, which
/// never searches).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefusalClass {
    pub reason: &'static str,
    pub signature: Option<&'static str>,
}

/// Whether `text` contains `phrase` (ASCII case-insensitive) with a non-word
/// character, or the text edge, on both sides.
#[must_use]
pub fn quotes_phrase(text: &str, phrase: &str) -> bool {
    let (hay, needle) = (text.to_ascii_lowercase(), phrase.to_ascii_lowercase());
    if needle.is_empty() {
        return false;
    }
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    hay.match_indices(&needle).any(|(at, _)| {
        !word(hay[..at].chars().next_back()) && !word(hay[at + needle.len()..].chars().next())
    })
}

/// The refusal class for `text`, or `None` when it is not a policy refusal.
#[must_use]
pub fn classify_refusal(text: &str) -> Option<RefusalClass> {
    if TRANSIENT.iter().any(|t| text.contains(t)) {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    let class = |reason, signature| Some(RefusalClass { reason, signature });
    for sig in SIGNATURES {
        if quotes_phrase(text, sig) {
            let reason = if sig.starts_with("merge commits") {
                "merge commits are not allowed on this repository (HTTP 405)"
            } else {
                "that merge method is not allowed on this repository (HTTP 405)"
            };
            return class(reason, Some(sig));
        }
    }
    if lower.contains("allowed method")
        || lower.contains("merge method")
        || lower.contains("merge-method")
    {
        return class("no merge method both the ruleset and the repo settings allow", None);
    }
    if lower.contains("repository rule violation") || lower.contains("ruleset") {
        return class("a branch ruleset refuses the merge", None);
    }
    if lower.contains("http 405") || lower.contains("405 method not allowed") {
        return class("the forge refuses the merge (HTTP 405)", None);
    }
    None
}

/// `#N` references in `text`, in order (bare `#123`; not `owner/repo#123`).
#[must_use]
pub fn hash_refs(text: &str) -> Vec<u32> {
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

/// The forge's own words, bounded and made inert for a comment and the
/// snapshot: the line quoting the failure (or the first line that classifies),
/// with backticks, angle brackets and control characters removed, so it can
/// sit inside a code span (which also keeps any `@` from pinging).
#[must_use]
pub fn raw_excerpt(body: &str) -> String {
    let line = body
        .lines()
        .find(|l| l.contains("Failed to merge PR #"))
        .or_else(|| body.lines().find(|l| classify_refusal(l).is_some()))
        .unwrap_or_default();
    let clean: String = line
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '`' | '<' | '>'))
        .collect();
    let clean = clean.trim();
    if clean.chars().count() > MAX_RAW {
        let cut: String = clean.chars().take(MAX_RAW).collect();
        format!("{cut}…")
    } else {
        clean.to_string()
    }
}

/// A refusal found in a PR's comments, before its incident is looked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub class: RefusalClass,
    /// [`raw_excerpt`] of the refusal comment.
    pub raw: String,
    /// Issue numbers the refusal comment itself names, in order, without the
    /// PR and its linked issue. Candidates only: the caller keeps the first
    /// that is an open issue.
    pub named: Vec<u32>,
}

impl Detected {
    /// The forge phrase an incident issue would quote, when the refusal is
    /// specific enough to search for one.
    #[must_use]
    pub fn signature(&self) -> Option<&'static str> {
        self.class.signature
    }
}

/// The refusal on PR `pr` (linked to `issue`), from its **trusted** comments
/// in forge order. The **latest** failed-merge report decides: a later report
/// that is not a policy refusal (a transient failure after the admin fixed
/// the settings) clears it. Later comments never name the incident.
#[must_use]
pub fn detect(comments: &[ForgeComment], pr: u32, issue: u32) -> Option<Detected> {
    let refusal = comments
        .iter()
        .rev()
        .find(|c| FAILURE_MARKERS.iter().any(|m| c.body.contains(m)))?;
    let class = classify_refusal(&refusal.body)?;
    let mut named = Vec::new();
    for n in hash_refs(&refusal.body) {
        if n != pr && n != issue && !named.contains(&n) {
            named.push(n);
        }
    }
    Some(Detected {
        class,
        raw: raw_excerpt(&refusal.body),
        named,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(body: &str) -> ForgeComment {
        ForgeComment {
            body: body.to_string(),
            ..ForgeComment::default()
        }
    }

    #[test]
    fn a_405_refusal_names_only_what_its_own_comment_names() {
        let comments = [
            c("Judge: approved"),
            c("**Champion: Merge Failed**\n\n```\nFailed to merge PR #9276: gh: Merge commits are not allowed on this repository. (HTTP 405)\n```\nSee #9115."),
            c("Stale-check re-date (#8508); approval invalidated (#5686)."),
        ];
        let d = detect(&comments, 9276, 8191).unwrap();
        assert!(d.class.reason.contains("405"), "{}", d.class.reason);
        assert_eq!(d.signature(), Some("merge commits are not allowed"));
        assert_eq!(d.named, vec![9115], "later comments never name the incident");
        assert!(d.raw.starts_with("Failed to merge PR #9276"), "{}", d.raw);
        assert!(!d.raw.contains('`'));
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
    fn no_failure_report_is_no_refusal_and_self_refs_are_not_candidates() {
        assert_eq!(detect(&[c("see #5")], 1, 2), None);
        let d = detect(
            &[c(
                "Failed to merge PR #7: Repository rule violations found (closes #3)",
            )],
            7,
            3,
        )
        .unwrap();
        assert!(d.named.is_empty());
        assert_eq!(hash_refs("a#1 x/y#2 #3 (#44)"), vec![3, 44]);
    }

    #[test]
    fn the_raw_excerpt_is_bounded_and_inert() {
        let long = format!("Failed to merge PR #1: <b>`{}`</b>\u{7}", "x".repeat(400));
        let raw = raw_excerpt(&long);
        assert!(raw.chars().count() <= MAX_RAW + 1);
        assert!(!raw.contains(['<', '>', '`', '\u{7}']), "{raw}");
    }

    #[test]
    fn only_the_specific_phrases_carry_a_signature() {
        for (text, sig) in [
            (
                "Failed to merge PR #1: Squash merges are not allowed (HTTP 405)",
                Some(SIGNATURES[1]),
            ),
            ("Failed to merge PR #1: rebase merges are not allowed.", Some(SIGNATURES[2])),
            ("Failed to merge PR #1: HTTP 405 Method Not Allowed", None),
            ("Failed to merge PR #1: no allowed merge method", None),
            ("Failed to merge PR #1: Repository rule violations found", None),
        ] {
            let d = detect(&[c(text)], 1, 2).expect(text);
            assert_eq!(d.signature(), sig, "{text}");
        }
    }

    #[test]
    fn the_phrase_match_is_word_bounded() {
        let p = SIGNATURES[0];
        assert!(quotes_phrase("gh: Merge commits are not allowed on this repository.", p));
        assert!(quotes_phrase("`merge commits are not allowed`", p));
        assert!(!quotes_phrase("remerge commits are not allowed", p));
        assert!(!quotes_phrase("merge commits are not allowedly", p));
        assert!(
            !quotes_phrase("See #4050 and #14050, v4051", "405"),
            "a bare number never matches inside a longer one"
        );
        assert!(!quotes_phrase("#4050", "405"));
        assert!(quotes_phrase("HTTP 405.", "405"));
    }
}
