//! Closing-keyword and partial-increment reference analysis for `merge-pr.sh`
//! (#8191, epic #7810 slice 1).
//!
//! These five functions decide whether merging a PR would close an issue the
//! PR only declared itself a *part of*. Getting that wrong reopens a correctly
//! closed issue, or silently closes one that is not finished — both of which
//! have happened.
//!
//! # Why these first
//!
//! They are the most fragile lines in `merge-pr.sh` per unit of length: five
//! stacked `awk`/`sed`/`grep -oiE` pipelines whose bug history is entirely
//! about *what the regex accidentally matched*.
//!
//! - **#5234** — a bare `grep` over the whole body matched a mid-sentence,
//!   backticked, conditional mention (`...and I will switch the reference to
//!   `Part of #4574``) and treated it as a declared intent, reopening an issue
//!   that had been correctly closed. The fix was to require the keyword be
//!   *line-leading* and to blank inline code spans first.
//! - The numbered-list ordinal trap: scanning a matched span for any digit run
//!   reads `3. Part of #789` as referencing **both** `3` and `789`. A body
//!   carrying that plus a genuine `Closes #3` would register #3 as a declared
//!   partial increment *and* a closing reference, and reopen it immediately
//!   after a correct close.
//!
//! Both are ordering bugs inside a pipeline, which is exactly the class that
//! is invisible to review and expensive to test in shell.
//!
//! # Fidelity
//!
//! Every function here is a byte-for-byte port of its shell counterpart's
//! observable output, pinned by a differential test that feeds the same corpus
//! to both implementations (`tests/merge_pr_refs_differential.rs`). Per
//! `defaults/docs/verification-recipes.md` §6 the corpus is generated ONCE and
//! read by both sides — never regenerated per-side from a shared seed, which
//! is how an earlier differential in this epic reported a divergence in the
//! code when the inputs had diverged instead.
//!
//! # The one translation that is not literal: `[[:space:]]`
//!
//! The shell matches with `grep -oiE`, which is **line-oriented** — it never
//! sees a newline, so its `[[:space:]]+` cannot span one. Rust's `regex`
//! operates on the whole string, where `[[:space:]]` *does* include `\n`.
//!
//! Translating the class literally therefore widened every pattern. The
//! differential caught it on its first run: for
//! `"Closes  #1\nCloses\t#2\nCloses\n#3\nCloses#4\n"` the shell yields
//! `[1, 2]` and a literal port yielded `[1, 2, 3]` — it read `Closes\n#3`,
//! spanning a line break, as a closing reference. A PR body with a dangling
//! `Closes` at end of line followed by an unrelated `#N` would have closed an
//! issue nobody referenced.
//!
//! So every `[[:space:]]` here is horizontal whitespace only. This is the same
//! shell-is-line-oriented divergence `dep_recheck::extract` documents for
//! #8011; it is a property of the tool, not of the pattern, and it does not
//! survive translation by itself.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;

/// Drop fenced code blocks, mirroring the shell's
/// `awk '/^[[:space:]]*```/ { infence = !infence; next } !infence { print }'`.
///
/// Note the shell toggles on *any* line whose first non-space run is
/// ```` ``` ````, and never inspects the fence's info string or length. An
/// unclosed fence therefore swallows the remainder of the text. That is
/// reproduced deliberately: it is the behaviour the closing-reference guard
/// has always had, and changing it here would change which references are
/// visible, which is not this port's job.
#[must_use]
pub fn strip_fenced_code_blocks(text: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        // ASCII horizontal whitespace only. `trim_start()` is Unicode-aware,
        // so a NBSP before the fence marker made the port see a fence where
        // `awk`'s `[[:space:]]` did not — inverting fence sense for the whole
        // rest of the body.
        if line
            .trim_start_matches([' ', '\t', '\x0B', '\x0C', '\r'])
            .starts_with("```")
        {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Blank inline code spans, mirroring `sed -E 's/`[^`]*`//g'`.
///
/// Non-greedy between backticks and left-to-right, so an odd trailing backtick
/// is left alone rather than consuming to end of text.
fn blank_inline_code(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    // `[^`\n]*`, NOT `[^`]*` — `sed` is per-line, so its span can never cross
    // a newline. The unrestricted class does, which is the SAME
    // shell-is-line-oriented divergence documented above for `[[:space:]]`,
    // one function away, and it produces errors in both directions:
    //
    //   "Use the `foo command\n\nPart of #5\n\nsee `bar`"  shell [5], Rust []
    //   "`a\nb` Part of #5"                                  shell [],  Rust [5]
    //
    // The trigger is an odd backtick count on an earlier line plus any later
    // backtick — a stray triple-backtick not at line start, or a typo. The
    // first shape makes a declaration invisible, which closes an issue that is
    // only partly done.
    RE.get_or_init(|| {
        Regex::new(
            r"`[^`
]*`",
        )
        .expect("static inline-code pattern")
    })
    .replace_all(text, "")
    .into_owned()
}

/// Issue numbers declared with a NON-closing partial-increment keyword
/// (`Part of #N` / `Contributes to #N`), deduped and ascending.
///
/// "Declaration" is deliberately narrower than "appears anywhere": the keyword
/// must be line-leading, optionally behind a list marker or blockquote. See the
/// module docs for the two incidents that shaped this.
#[must_use]
pub fn partial_increment_refs(text: &str) -> Vec<u64> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?im)^[[:blank:]\x0B\x0C\r]*([-*+>]|[0-9]+\.)?[[:blank:]\x0B\x0C\r]*(Part of|Contributes to)[[:blank:]\x0B\x0C\r]+#([0-9]+)")
            .expect("static partial-increment pattern")
    });
    let cleaned = blank_inline_code(&strip_fenced_code_blocks(text));

    // Capture group 3 only — NOT a digit scan over the whole match, which would
    // also read a numbered-list marker's own ordinal as an issue number.
    let mut set: BTreeSet<u64> = BTreeSet::new();
    for caps in re.captures_iter(&cleaned) {
        if let Some(n) = caps.get(3).and_then(|m| m.as_str().parse::<u64>().ok()) {
            set.insert(n);
        }
    }
    set.into_iter().collect()
}

/// Issue numbers referenced with a GitHub CLOSING keyword anywhere in `text`,
/// deduped and ascending.
///
/// The keyword set and the `\b` guard are deliberately identical to the one
/// `forge_pr_close_targets()` uses on its Gitea branch — `\b` is what stops
/// `Discloses #N` matching `close`.
///
/// This exists alongside the authoritative GraphQL `closingIssuesReferences`
/// because that field is GraphQL-only, and the incident it guards against
/// happened while GraphQL quota was exhausted. A quota-free text signal is the
/// one that still works in the conditions where the bug bites.
#[must_use]
pub fn closing_refs(text: &str) -> Vec<u64> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)\b(close[sd]?|fix(e[sd])?|resolve[sd]?)\b[[:blank:]\x0B\x0C\r]+#([0-9]+)")
            .expect("static closing-keyword pattern")
    });
    let mut set: BTreeSet<u64> = BTreeSet::new();
    for caps in re.captures_iter(text) {
        if let Some(n) = caps.get(3).and_then(|m| m.as_str().parse::<u64>().ok()) {
            set.insert(n);
        }
    }
    set.into_iter().collect()
}

/// Tri-state result of scanning `text` for closing-keyword references to a
/// single issue — see [`closing_ref_negation_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosingRefNegationStatus {
    /// At least one closing-keyword reference to the issue was found, and it
    /// (or a later one) is not negated — a genuine closing intent.
    Unnegated,
    /// At least one closing-keyword reference to the issue was found, and
    /// every one of them is negated (`does not fix #N`).
    NegatedOnly,
    /// No closing-keyword reference to the issue appears in the text at all.
    /// Distinct from `NegatedOnly` on purpose: a body that never mentions the
    /// issue (e.g. it is linked only through the PR's Development sidebar,
    /// or via `Fixes owner/repo#N` / `Closes: #N`, forms this regex cannot
    /// see) must not be treated as a disclaimed close.
    NoReference,
}

/// Classifies `text`'s closing-keyword references to `issue` (#1057) as
/// [`ClosingRefNegationStatus::Unnegated`], `NegatedOnly`, or `NoReference` —
/// backs Champion's "Verify Issue Auto-Close" cross-check.
///
/// GitHub's `closingIssuesReferences` parser, and [`closing_refs`] above, read
/// `does not fix #909` exactly like `fixes #909`. A PR whose body said
/// "**does not fix #909** — left open for its owner" auto-closed #909 anyway
/// (example-org/tool-repo#1057). This is the re-check a caller runs on each
/// raw candidate before treating it as a genuine close.
///
/// Negation words: `not`, `never`, or any `n't` contraction (`doesn't`,
/// `won't`, …, straight or curly apostrophe). The negation must sit
/// immediately before the closing keyword — at most two intervening words
/// (`does not fix`, `doesn't fully fix`, `will not close`) — rather than
/// anywhere earlier in the same clause. A whole-clause scan would read
/// "If the lock isn't held we now return early, which fixes #42" as negated
/// even though `isn't` has nothing to do with `fixes #42`; scoping the
/// window to the words right before the keyword keeps that unnegated while
/// still catching every #1057 shape. "Clause" is approximated by splitting
/// on `.`, `!`, `?`, `;` and newlines, so a negation in one clause never
/// suppresses a genuine close in another. Within a clause only the text up
/// to the FIRST matching `#N` counts, so text after a genuine close cannot
/// retroactively negate it.
#[must_use]
pub fn closing_ref_negation_status(text: &str, issue: u64) -> ClosingRefNegationStatus {
    static NEG_NEAR_END: OnceLock<Regex> = OnceLock::new();
    let neg_near_end = NEG_NEAR_END.get_or_init(|| {
        Regex::new(r"(?i)(?:\bnot\b|\bnever\b|\w+n['’]t\b)(?:\s+\S+){0,2}\s*$")
            .expect("static negation-window pattern")
    });
    let kw = Regex::new(&format!(
        r"(?i)\b(close[sd]?|fix(e[sd])?|resolve[sd]?)\b[[:blank:]\x0B\x0C\r]+#{issue}\b"
    ))
    .expect("closing-reference pattern");

    let mut saw_reference = false;
    for clause in text.split(['.', '!', '?', ';', '\n']) {
        if let Some(m) = kw.find(clause) {
            saw_reference = true;
            let prefix = &clause[..m.start()];
            if !neg_near_end.is_match(prefix) {
                return ClosingRefNegationStatus::Unnegated;
            }
        }
    }
    if saw_reference {
        ClosingRefNegationStatus::NegatedOnly
    } else {
        ClosingRefNegationStatus::NoReference
    }
}

/// Whether `text` carries at least one closing-keyword reference to `issue`
/// that is NOT negated. Convenience wrapper over
/// [`closing_ref_negation_status`] for callers that only need the boolean.
#[must_use]
pub fn has_unnegated_closing_ref(text: &str, issue: u64) -> bool {
    closing_ref_negation_status(text, issue) == ClosingRefNegationStatus::Unnegated
}

/// Render matched snippets the way the shell does: sorted, deduped, and joined
/// with `", "` so the caller can drop the result straight into a quoted
/// message.
fn render_snippets(mut found: Vec<String>) -> String {
    found.sort();
    found.dedup();
    found.join("\", \"")
}

/// The literal closing-keyword snippets in `text` that reference `issue`.
///
/// Empty when the text carries no such reference — which is how the caller
/// tells WHICH source (body vs. commit messages) is at fault.
#[must_use]
pub fn closing_ref_snippets(text: &str, issue: u64) -> String {
    let re = Regex::new(&format!(
        r"(?i)\b(close[sd]?|fix(e[sd])?|resolve[sd]?)\b[[:blank:]\x0B\x0C\r]+#{issue}\b"
    ))
    .expect("closing-snippet pattern");
    render_snippets(re.find_iter(text).map(|m| m.as_str().to_string()).collect())
}

/// The literal `Part of #N` / `Contributes to #N` declaration snippets in
/// `text` for `issue`, with leading whitespace trimmed.
///
/// Runs the identical fence/inline-code stripping as
/// [`partial_increment_refs`], so a quoted snippet always matches what was
/// actually matched rather than a code-block artifact.
#[must_use]
pub fn partial_increment_ref_snippets(text: &str, issue: u64) -> String {
    let re = Regex::new(&format!(
        r"(?im)^[[:blank:]\x0B\x0C\r]*([-*+>]|[0-9]+\.)?[[:blank:]\x0B\x0C\r]*(Part of|Contributes to)[[:blank:]\x0B\x0C\r]+#{issue}\b"
    ))
    .expect("partial-increment-snippet pattern");
    let cleaned = blank_inline_code(&strip_fenced_code_blocks(text));
    render_snippets(
        re.find_iter(&cleaned)
            .map(|m| m.as_str().trim_start().to_string())
            .collect(),
    )
}

// --- Backticked partial-increment trailer detection (#5690, ported #8831) --
//
// #5234's inline-code-span exclusion above is CORRECT and unchanged: a
// backticked `Part of #N` is a hypothetical mention, not a declared intent.
// The gap it leaves is that a Builder can write the convention's exact
// trailer text inside a code span — it reads as "a literal piece of syntax,
// so put it in backticks" — and the PR then looks completely right to a
// human reviewer and to Judge while silently defeating the automation:
// [`partial_increment_refs`] returns nothing, so the #3667 `loom:building` ->
// `loom:issue` reset no-ops with no log line anywhere and the issue is
// stranded at `loom:building` indefinitely. That is exactly what happened
// merging PR #5686 into #5240.
//
// The remedy is DETECTION, not a parser change: [`backticked_partial_increment_warnings`]
// below is a non-blocking pre-merge warning (same advisory style as the
// #4569/#4595 conflict warnings), so the person running the merge sees "this
// body looks like it is trying to declare a partial increment, but the
// declaration will not parse".
//
// The shape matched is deliberately MUCH narrower than the declaration anchor
// in `partial_increment_refs`: the backticked trailer must be the ENTIRE line
// (modulo an optional list/blockquote marker, surrounding whitespace and one
// trailing punctuation mark). That is the shape a Builder produces when they
// MEANT to declare, and it excludes the two prose shapes that must stay
// silent — a mid-sentence mention ("...I will switch the reference to `Part
// of #4574`.") and a line that merely lists backticked trailers as examples
// ("`Part of #123` / `Contributes to #456` — non-closing trailers"). Fenced
// code blocks are stripped first, so a documentation example never warns —
// but inline code spans are NOT blanked, since they are the whole shape being
// matched (unlike `partial_increment_refs`, which blanks them to look past
// them).

/// Head of the whole-line backticked-trailer shape: optional list/blockquote
/// marker(s), a code span opening, the keyword, then `#`. Split from the tail
/// so the ref extractor and the snippet extractor share one expression —
/// there is no way for "what warned" and "what is quoted" to drift apart.
const BACKTICKED_PARTIAL_HEAD: &str = r"^[[:blank:]\x0B\x0C\r]*(?:[-*+][[:blank:]\x0B\x0C\r]+|[0-9]+\.[[:blank:]\x0B\x0C\r]+|>[[:blank:]\x0B\x0C\r]*)*`+[[:blank:]\x0B\x0C\r]*(?:part of|contributes to)[[:blank:]\x0B\x0C\r]+#";
/// Tail of the shape: the closing code span, an optional single trailing
/// punctuation mark, then end of line.
const BACKTICKED_PARTIAL_TAIL: &str =
    r"[[:blank:]\x0B\x0C\r]*`+[[:blank:]\x0B\x0C\r]*[.,;:]?[[:blank:]\x0B\x0C\r]*$";

/// Issue numbers that appear ONLY as a whole-line, code-span-wrapped
/// `Part of #N` / `Contributes to #N` trailer, deduped and ascending. Like
/// [`partial_increment_refs`], `#N` is captured directly rather than scanned
/// for out of the whole match, so a numbered-list marker's own ordinal
/// (`` 3. `Part of #789` ``) cannot leak in as an issue number.
#[must_use]
pub fn backticked_partial_increment_trailer_refs(text: &str) -> Vec<u64> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(&format!("(?im){BACKTICKED_PARTIAL_HEAD}([0-9]+){BACKTICKED_PARTIAL_TAIL}"))
            .expect("static backticked-partial-trailer pattern")
    });
    let cleaned = strip_fenced_code_blocks(text);
    let mut set: BTreeSet<u64> = BTreeSet::new();
    for caps in re.captures_iter(&cleaned) {
        if let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<u64>().ok()) {
            set.insert(n);
        }
    }
    set.into_iter().collect()
}

/// The literal offending line(s) in `text` carrying a backticked trailer for
/// `issue`, trimmed and joined `", "` in the order they appear.
///
/// Deliberately NOT sorted/deduped like [`render_snippets`] — the shell this
/// ports emitted them via a plain `awk` concatenation in encounter order, and
/// this matches that exactly rather than "improving" it.
#[must_use]
pub fn backticked_partial_increment_trailer_snippets(text: &str, issue: u64) -> String {
    let re = Regex::new(&format!("(?im){BACKTICKED_PARTIAL_HEAD}{issue}{BACKTICKED_PARTIAL_TAIL}"))
        .expect("backticked-partial-trailer-snippet pattern");
    let cleaned = strip_fenced_code_blocks(text);
    re.find_iter(&cleaned)
        .map(|m| m.as_str().trim().to_string())
        .collect::<Vec<_>>()
        .join("\", \"")
}

/// The non-blocking pre-merge warning text for #5690: one finding (two lines)
/// per issue a backticked trailer names that [`partial_increment_refs`] does
/// NOT — an issue named by BOTH shapes is not warned about, since the #3667
/// reset fires for it regardless. Empty when there is nothing to warn about.
///
/// `dry_run` prefixes each line `[dry-run] `, matching the #4569/#4595
/// conflict warnings' contract: report the would-be outcome without claiming
/// a merge is happening.
#[must_use]
pub fn backticked_partial_increment_warnings(text: &str, pr_number: &str, dry_run: bool) -> String {
    let declared: BTreeSet<u64> = partial_increment_refs(text).into_iter().collect();
    let dr = if dry_run { "[dry-run] " } else { "" };
    let mut out = String::new();
    for issue in backticked_partial_increment_trailer_refs(text) {
        if declared.contains(&issue) {
            continue;
        }
        let snippet = backticked_partial_increment_trailer_snippets(text, issue);
        out.push_str(&format!(
            "{dr}Backticked partial-increment trailer (#5690): PR #{pr_number}'s body carries a whole-line `Part of`/`Contributes to` reference to #{issue} wrapped in a code span (\"{snippet}\"), which is NOT read as a declaration — inline code spans are deliberately excluded (#5234) so a hypothetical mention cannot be mistaken for declared intent.\n"
        ));
        out.push_str(&format!(
            "  {dr}Consequence: the automatic `loom:building` -> `loom:issue` reset (#3667) will NOT run for #{issue} on merge, and nothing else logs that it was skipped — #{issue} would sit at `loom:building` until a stale-claim pass reclaims it. If #{issue} really is a partial increment, edit the PR body so the trailer is PLAIN TEXT on its own line (no backticks), then re-run this merge. If the mention was hypothetical, ignore this — nothing is blocked.\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests;
