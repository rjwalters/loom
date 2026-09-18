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

#[cfg(test)]
mod tests;
