//! `extract-refs` — the reference extraction behind "Checking Operator-Only
//! Premises" (#4963) (epic #7810, PR 4).
//!
//! # The self-perpetuating loop this closes
//!
//! The bot's own "premise possibly stale" comment quotes the matched phrase
//! back into the thread. A naive `[.body] + [.comments[].body]` scan — the old
//! inline `curator.md` shell — re-matches its own prior report forever, even
//! after the body itself is fixed. So references come from the issue **body**
//! always, plus only those comments that are neither authored by the automation
//! identity nor carrying one of its own markers.

use regex::Regex;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::sync::OnceLock;

/// Default `--bot-login`.
pub const DEFAULT_BOT_LOGIN: &str = "loom-fleet-dispatch";

/// Markers that identify the automation's own re-check comments. Excluded
/// belt-and-suspenders, on top of the author check: a comment carrying one was
/// written by this mechanism whatever login the forge reports for it.
const OWN_MARKERS: [&str; 2] = [
    "<!-- curator:dep-recheck:",
    "<!-- curator:operator-premise-recheck:",
];

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Author {
    #[serde(default)]
    pub login: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Comment {
    #[serde(default)]
    pub author: Author,
    #[serde(default)]
    pub body: String,
}

/// The `--stdin` document — the same shape `gh issue view --json body,comments`
/// returns, so a live-mode fixture can be captured verbatim.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Input {
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub comments: Vec<Comment>,
}

/// The fixed machine-readable phrasings, reused verbatim from
/// `detect-dependency-cycle.sh` / `warn-operator-gated.sh` rather than
/// inventing a second vocabulary. A bare prose mention (a backtick-quoted
/// `owner/repo#123`, say) deliberately does not count.
fn phrase_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(Blocked by|Depends on|Requires|\*\*Epic\*\*)[*_:\s]*#([0-9]+)")
            .expect("static dependency-phrase pattern")
    })
}

/// Normalise a login for comparison: lower-cased, with a leading `app/` or a
/// trailing `[bot]` stripped.
///
/// Different `gh` views normalise a GitHub App's login differently — compare
/// `judge-fallback-guard.sh`'s `app/loom-fleet-dispatch` PR-author check with
/// the bare `loom-fleet-dispatch` that `gh issue view --json comments` reports.
/// Matching only one spelling would let the loop back in through the other.
#[must_use]
pub fn normalise_login(login: &str) -> String {
    let lower = login.to_ascii_lowercase();
    let no_prefix = lower.strip_prefix("app/").unwrap_or(&lower);
    no_prefix
        .strip_suffix("[bot]")
        .unwrap_or(no_prefix)
        .to_string()
}

/// Whether a comment may contribute references.
#[must_use]
pub fn comment_counts(comment: &Comment, bot_login: &str) -> bool {
    if normalise_login(&comment.author.login) == normalise_login(bot_login) {
        return false;
    }
    !OWN_MARKERS.iter().any(|m| comment.body.contains(m))
}

/// The reference numbers, sorted numerically and deduplicated, space-joined.
///
/// `sort -un` — **numeric**, unlike the line sorts elsewhere in this port.
/// This list is an argument passed on to `operator-premise`, not a hashed
/// rendering, so its ordering is the shell's own and worth keeping exact.
#[must_use]
pub fn extract(input: &Input, bot_login: &str) -> String {
    let mut text = input.body.clone();
    for c in &input.comments {
        if comment_counts(c, bot_login) {
            text.push('\n');
            text.push_str(&c.body);
        }
    }

    let numbers: BTreeSet<u64> = phrase_re()
        .captures_iter(&text)
        .filter_map(|c| c.get(2)?.as_str().parse().ok())
        .collect();

    numbers
        .into_iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests;
