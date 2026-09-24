//! Backticked partial-increment trailer detection for `merge-pr.sh` (#8796).
//!
//! Not a port. [`refs`](super::refs) decides what *is* a declaration; this
//! module decides when a body looks like it was **trying** to declare one and
//! failed silently.
//!
//! # The gap
//!
//! [`refs::partial_increment_refs`](super::refs::partial_increment_refs) blanks
//! inline code spans before matching, on purpose (#5234): a backticked
//! `Part of #N` is a hypothetical mention, not a declared intent. That
//! exclusion is correct and is **not** changed here.
//!
//! The gap it leaves is that a Builder can write the convention's exact trailer
//! text inside a code span — it reads as "a literal piece of syntax, so put it
//! in backticks" — and the PR then looks completely right to a human reviewer
//! and to Judge while silently defeating the automation: the ref list comes
//! back empty, so the #3667 `loom:building` -> `loom:issue` reset no-ops with
//! no log line anywhere and the family/epic issue is stranded at
//! `loom:building` indefinitely.
//!
//! That happened downstream on rjwalters/kicad-tools: merging PR #5686 into
//! issue #5240, whose body carried `` `Part of #5240` `` on its own line. The
//! reset never fired; the label swap and an explanatory comment had to be
//! applied by hand. Two prior partial-increment merges on that same issue used
//! the plain-text form and reset correctly — a body-shape trap, not a
//! regression.
//!
//! # The remedy is DETECTION, not a parser change
//!
//! A non-blocking pre-merge warning, in the same advisory style as the
//! #4569/#4595 conflict warnings, so the person (or agent) running the merge
//! sees "this body looks like it is trying to declare a partial increment, but
//! the declaration will not parse". Nothing is blocked and nothing is mutated.
//!
//! # Why the matched shape is much narrower than the declaration anchor
//!
//! The backticked trailer must be the **entire line** — modulo an optional
//! list/blockquote marker, surrounding whitespace, and one trailing punctuation
//! mark. That is the shape a Builder produces when they MEANT to declare, and
//! it excludes the two prose shapes that must stay silent:
//!
//! - the mid-sentence #5234 mention (`...I will switch the reference to
//!   `` `Part of #4574` ``.`), and
//! - a line that merely lists backticked trailers as examples (`` `Part of
//!   #123` `` / `` `Contributes to #456` `` — non-closing trailers).
//!
//! Warning on either of those would train operators to ignore this warning
//! entirely, which costs more than the silence it replaces. Fenced code blocks
//! are stripped first, so a documentation example never warns.
//!
//! # Why this lives in Rust rather than in `merge-pr.sh`
//!
//! `merge-pr.sh` is frozen by the file-size AND shell-budget ratchets (see
//! [`super`]), so the fixes it keeps needing must be net-zero or smaller. The
//! script's call site (inside `_check_partial_increment_close_conflict`, folded
//! onto the pre-existing early-return line to add zero net "contract" shell
//! lines, #8831) pipes the body here and re-emits each returned line through
//! its own `warning`. The call is advisory, so the script deliberately
//! swallows a failure: on a `loom-daemon` predating this subcommand the
//! warning is simply absent, which is exactly the pre-#8796 behaviour and
//! never blocks a merge.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;

use super::refs;

/// Horizontal whitespace only — never `\n`.
///
/// The same shell-is-line-oriented divergence [`refs`](super::refs) documents
/// at length: `grep`/`sed` cannot span a line break and Rust's `[[:space:]]`
/// can, so translating the class literally silently widens every pattern.
const BLANK: &str = r"[[:blank:]\x0B\x0C\r]";

/// A whole line whose entire content is a code-span-wrapped partial-increment
/// trailer. Capture group 3 is the issue number.
///
/// Built once and shared by both public functions, so "what warned" and "what
/// is quoted" cannot drift apart.
fn whole_line_backticked_trailer() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"(?im)^{BLANK}*([-*+>]|[0-9]+\.)?{BLANK}*`+{BLANK}*(Part of|Contributes to){BLANK}+#([0-9]+){BLANK}*`+{BLANK}*[.,;:!?]?{BLANK}*$"
        ))
        .expect("static backticked-trailer pattern")
    })
}

/// Issue numbers that appear as a whole-line, code-span-wrapped trailer in
/// `text`, deduped and ascending.
///
/// Capture group 3 only — NOT a digit scan over the whole match, which would
/// also read a numbered-list marker's own ordinal as an issue number
/// (`3. `` `Part of #789` `` -> `3` and `789`). That trap is the one #5234's
/// sibling fix exists for, and a detector that fell into it would warn about an
/// issue nobody mentioned.
#[must_use]
pub fn backticked_trailer_refs(text: &str) -> Vec<u64> {
    let cleaned = refs::strip_fenced_code_blocks(text);
    let mut set: BTreeSet<u64> = BTreeSet::new();
    for caps in whole_line_backticked_trailer().captures_iter(&cleaned) {
        if let Some(n) = caps.get(3).and_then(|m| m.as_str().parse::<u64>().ok()) {
            set.insert(n);
        }
    }
    set.into_iter().collect()
}

/// The literal offending line(s) in `text` that carry a backticked trailer for
/// `issue`, trimmed and joined `snippet", "snippet` — the same rendering the
/// rest of this guard's warnings use, so the caller can drop the result
/// straight into a quoted message.
#[must_use]
pub fn backticked_trailer_snippets(text: &str, issue: u64) -> String {
    let cleaned = refs::strip_fenced_code_blocks(text);
    let mut found: Vec<String> = whole_line_backticked_trailer()
        .captures_iter(&cleaned)
        .filter(|caps| caps.get(3).is_some_and(|m| m.as_str() == issue.to_string()))
        .map(|caps| {
            caps.get(0)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default()
        })
        .collect();
    found.sort();
    found.dedup();
    found.join("\", \"")
}

/// The complete, ready-to-print advisory warnings for `text`, two lines per
/// affected issue, or an empty vector when there is nothing to say.
///
/// An issue named by BOTH a backticked line and a real plain-text trailer is
/// **not** warned about: the reset will fire for it regardless, and a warning
/// there would be pure noise.
///
/// `dry_run` mirrors the conflict warnings' contract — report the would-be
/// outcome without claiming a merge is happening.
#[must_use]
pub fn backticked_trailer_warnings(text: &str, pr: u64, dry_run: bool) -> Vec<String> {
    let declared: BTreeSet<u64> = refs::partial_increment_refs(text).into_iter().collect();
    let dr = if dry_run { "[dry-run] " } else { "" };

    let mut out = Vec::new();
    for issue in backticked_trailer_refs(text) {
        if declared.contains(&issue) {
            continue;
        }
        let snippet = backticked_trailer_snippets(text, issue);
        out.push(format!(
            "{dr}Backticked partial-increment trailer (#8796): PR #{pr}'s body carries a \
             whole-line `Part of`/`Contributes to` reference to #{issue} wrapped in a code span \
             (\"{snippet}\"), which is NOT read as a declaration — inline code spans are \
             deliberately excluded (#5234) so a hypothetical mention cannot be mistaken for \
             declared intent."
        ));
        out.push(format!(
            "  {dr}Consequence: the automatic `loom:building` -> `loom:issue` reset (#3667) will \
             NOT run for #{issue} on merge, and nothing else logs that it was skipped — #{issue} \
             would sit at `loom:building` until a stale-claim pass reclaims it. If #{issue} \
             really is a partial increment, edit the PR body so the trailer is PLAIN TEXT on its \
             own line (no backticks), then re-run this merge. If the mention was hypothetical, \
             ignore this — nothing is blocked."
        ));
    }
    out
}

#[cfg(test)]
mod tests;
