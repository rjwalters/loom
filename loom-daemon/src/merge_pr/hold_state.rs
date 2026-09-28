//! The Champion `champion:hold-state` staleness warning (#7419 AC #3, a slice
//! of #8191).
//!
//! # What it watches
//!
//! When Champion declines to auto-merge a Judge-approved PR it posts a
//! merge-risk hold notice, and records the head it held against as a machine
//! marker inside that notice:
//!
//! ```text
//! <!-- champion:merge-risk-hold -->
//! <!-- champion:hold-state head=<sha> -->
//! **Champion: Holding for Human Merge**
//! ```
//!
//! `merge-pr.sh` reads it back at merge time, on the path where `loom:pr` IS
//! present. A recorded head that differs from the head about to merge means
//! the hold/approval state was recorded against a different tree, so the
//! operator gets a WARNING — never a block. `loom:pr`'s presence already means
//! Judge approved SOME head; this only says "possibly not this one", which is
//! a softer signal than [`super::loom_pr_guard`]'s missing-label case and is
//! deliberately not merge-fatal.
//!
//! # Two defects this port fixes
//!
//! The retired shell was `grep -o 'champion:hold-state head=[0-9a-f]*' | tail
//! -1 | sed -n 's/.*head=\([0-9a-f]*\)/\1/p'` over every comment body
//! concatenated. Both of its failure modes lose the warning silently, which is
//! the only direction that matters for a check nothing else duplicates.
//!
//! **1. An empty capture masks a real marker.** `[0-9a-f]*` matches the EMPTY
//! string, so the literal documentation line `<!-- champion:hold-state
//! head=<sha> -->` — which appears verbatim in `champion-pr-merge.md`, in
//! `merge-pr.sh`'s own comments, and therefore in any PR comment quoting
//! either — is a match whose capture is empty. `tail -1` takes the LAST match,
//! so one quoted example posted after a genuine hold erases the genuine hold's
//! SHA and the staleness check returns silently. [`recorded_head`] requires
//! `[0-9a-f]+` and takes the last match that actually carries a SHA.
//!
//! **2. A bare substring anywhere is authoritative.** The shell matched the
//! marker text wherever it appeared — mid-sentence, backticked, inside a
//! fenced example. Champion's own reader already learned this lesson on the
//! sibling `<!-- champion:merge-risk-hold -->` marker (#5371: "a later comment
//! quoting this marker in prose must never be mistaken for the hold notice's
//! own comment") and answered it with `startswith`. This port applies the same
//! narrowing: a marker counts only when it sits inside an HTML comment that
//! opens and closes **on one line**, which is exactly and only the shape the
//! producer writes.
//!
//! # The divergence deliberately NOT taken
//!
//! [`super::refs`] strips fenced code blocks before reading its declarations,
//! and the same hazard exists here — a fenced example of the marker still
//! counts. It is not stripped, on purpose. Fence state is not line-local, and
//! the input here is every comment body on the PR concatenated with no
//! separator, so a single unclosed ``` in any earlier comment would swallow
//! every later one, including a real hold notice. Losing a genuine marker is
//! the worse error (the warning simply never appears), and the HTML-comment
//! anchor above already excludes the far more common prose/backtick shapes.
//! Recorded here rather than left to be rediscovered.

use regex::Regex;
use std::sync::OnceLock;

/// The only stdout a caller may treat as "nothing stale to report".
///
/// A sentinel rather than silence, matching [`super::labels::CLEAN`] and
/// [`super::loom_pr_guard::CLEAN`]. This check is advisory — its caller
/// proceeds either way — so the sentinel is not a safety gate here; it is how
/// the shell tells "the check ran and found nothing" from "the binary printed
/// nothing because it never ran", which are different things to log.
pub const CLEAN: &str = "LOOM-HOLD-STATE-CLEAN";

/// The marker, as it appears inside the HTML comment the producer writes.
///
/// `[0-9a-f]+`, not `*`: see defect 1 in the module docs. Lowercase-only,
/// matching the retired shell and the producer (`$HEAD_SHA` from the forge is
/// always lowercase hex) — widening it would make a prose "HEAD=" line match.
fn marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"champion:hold-state head=([0-9a-f]+)").expect("static hold-state pattern")
    })
}

/// The inner text of every HTML comment that both opens and closes on `line`.
///
/// Line-local by construction: a `<!--` with no `-->` after it on the same
/// line yields nothing. That is the point — the input is many comment bodies
/// concatenated, so any multi-line scan lets one malformed comment change how
/// a later one is read.
fn html_comment_spans(line: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find("<!--") {
        let after = &rest[open + 4..];
        let Some(close) = after.find("-->") else {
            break;
        };
        spans.push(&after[..close]);
        rest = &after[close + 3..];
    }
    spans
}

/// The head SHA the most recent `champion:hold-state` marker recorded.
///
/// `comments` is every comment body on the PR, concatenated — what
/// `forge_get_pr_comments` returns. "Most recent" is the LAST qualifying
/// marker in that stream, matching the retired shell's `tail -1` and the
/// forge's chronological comment order.
#[must_use]
pub fn recorded_head(comments: &str) -> Option<String> {
    let mut last = None;
    for line in comments.lines() {
        for span in html_comment_spans(line) {
            for caps in marker_re().captures_iter(span) {
                last = Some(caps[1].to_string());
            }
        }
    }
    last
}

/// The staleness warning for this PR, or `None` when there is nothing to say.
///
/// `None` covers all three quiet cases the shell had: no marker at all, and a
/// marker whose SHA is the head about to merge.
#[must_use]
pub fn assess(pr: &str, comments: &str, head_sha: &str) -> Option<String> {
    let recorded = recorded_head(comments)?;
    if recorded == head_sha {
        return None;
    }
    Some(message(pr, &recorded, head_sha))
}

/// The warning text, byte-identical to the retired shell's.
///
/// Held stable deliberately: `test-merge-pr-loom-pr-label-guard.sh` asserts
/// on its wording, and an operator greps for it.
#[must_use]
pub fn message(pr: &str, recorded: &str, head_sha: &str) -> String {
    format!(
        "champion:hold-state marker recorded head={recorded}, but PR #{pr}'s current head is \
{head_sha} — the hold/approval state may have been recorded against a different tree than the \
one about to merge. loom:pr's presence means Judge approved SOME head; verify it still covers \
this one before proceeding."
    )
}

#[cfg(test)]
mod tests;
