//! `named-dependency` — the `## Dependencies` checklist fingerprint (#7314)
//! (epic #7810, PR 4).
//!
//! Covers the shape `dep-recheck` cannot see: a checklist item naming a
//! *different*, non-closing issue or PR as a prerequisite. #6335 was blocked on
//! #6333, which carries no `Closes #6335`, so it never appeared in
//! `closedByPullRequestsReferences` at all.

use super::extract::DEPENDENCY_PHRASES;
use crate::short_hash::short_sha16;
use regex::Regex;
use serde::Deserialize;
use std::sync::OnceLock;

/// One checklist entry.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct Dep {
    pub number: i64,
    #[serde(default)]
    pub checked: bool,
    /// The referenced issue's or PR's own state. `None` for a checked item —
    /// never looked up, never consulted.
    #[serde(default)]
    pub state: Option<String>,
}

/// The `--stdin` document.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Input {
    pub deps: Vec<Dep>,
}

/// What this pass concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub verdict: String,
    pub deps: String,
    pub conclusion_hash: String,
}

/// A checklist item: `- [ ] #123: ...` or `* [ ] #123: ...`, with an optional
/// dependency phrase (`Blocked by`, `Depends on`, `Requires`, `**Epic**`)
/// and/or a `PR `/`Issue ` token before the `#N`, in that order. All of the
/// optional parts are case-insensitive, and either may appear without the
/// other (`- [ ] Blocked by PR #3` matches both).
///
/// The optional token is #7501: curator prose naturally varies ("PR #N",
/// "Issue #N"), and silently dropping such an item produces a false
/// `VERDICT=clear` — which can unblock a Builder that is genuinely blocked.
/// Being slightly too permissive is the better failure direction here.
///
/// The optional phrase is #8119, and exists for exactly the same reason one
/// step further out: `- [ ] Blocked by #6333: prerequisite` is the single most
/// natural way to write a prerequisite, [`super::extract`] already reads it as
/// a reference, and this matcher used to drop it — so the two subcommands
/// disagreed about what counts as a dependency and the more permissive one was
/// not the one deciding the verdict. The vocabulary is
/// [`super::extract::DEPENDENCY_PHRASES`], shared rather than re-spelled, per
/// that module's own "reused verbatim … rather than inventing a second
/// vocabulary" precedent.
///
/// Two deliberate narrowings relative to [`super::extract`]'s `phrase_re`:
/// the separator between phrase and `#N` is `[*_: \t]*` rather than
/// `[*_:\s]*`, so a match can never span a newline (a checklist *item* is a
/// line), and the phrase must sit directly after the checkbox rather than
/// anywhere in the item's prose — `- [ ] #3: blocked by #99` still yields only
/// `#3`, not `#99`.
///
/// **Divergence from the pre-port shell (#8011, kept intentionally):** the
/// shell's `_extract_named_deps` matched only a literal `-` bullet;
/// `[-*]` here also accepts `*`, which is valid GitHub task-list syntax and
/// shows up in hand-written Curator checklists. Narrowing back to `-`-only
/// would silently drop a real `* [ ] #N: ...` dependency and manufacture a
/// false `VERDICT=clear` — the same worse-failure-direction argument as the
/// `PR `/`Issue ` token above — so the broader match is kept rather than
/// narrowed. This changes `CONCLUSION_HASH` for any body that happens to use
/// `*` bullets in its `## Dependencies` section.
fn item_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // The bullet and its box, then an optional dependency phrase (#8119),
        // then an optional `PR `/`Issue ` token (#7501), then the reference.
        // Kept on one line: the phrases themselves contain literal spaces, so
        // `(?x)` free-spacing mode would silently mangle the shared vocabulary.
        let pattern = format!(
            r"(?im)^[ \t]*[-*][ \t]*\[([ xX])\][ \t]*(?:(?:{DEPENDENCY_PHRASES})[*_: \t]*)?(?:(?:pr|issue)[ \t]+)?#([0-9]+)"
        );
        Regex::new(&pattern).expect("static checklist pattern")
    })
}

/// The `## Dependencies` (or `### Dependencies`) section of a body.
///
/// Scoped so an unrelated `#N` elsewhere in the issue is never read as a named
/// dependency. H2 **or** H3, because Curators file both shapes in practice
/// (#7503) — and again, a false "clear" is the worse direction than being a
/// little permissive.
#[must_use]
pub fn dependencies_section(body: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in body.lines() {
        if is_dependencies_heading(line) {
            inside = true;
            continue;
        }
        if inside && is_heading_up_to_h3(line) {
            inside = false;
        }
        if inside {
            out.push(line);
        }
    }
    out.join("\n")
}

/// `/^#{2,3}[[:space:]]+Dependencies[[:space:]]*$/` — exactly, including the
/// end anchor: `## Dependencies (deferred)` is deliberately not this section.
fn is_dependencies_heading(line: &str) -> bool {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if !(2..=3).contains(&hashes) {
        return false;
    }
    let rest = &line[hashes..];
    if !rest.starts_with([' ', '\t']) {
        return false;
    }
    rest.trim() == "Dependencies"
}

/// `/^#{1,3}[[:space:]]/` — a same-or-shallower heading ends the section.
fn is_heading_up_to_h3(line: &str) -> bool {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    (1..=3).contains(&hashes) && line[hashes..].starts_with([' ', '\t'])
}

/// The checklist entries declared in `body`'s `## Dependencies` section.
///
/// Returns `(number, checked)` pairs in document order.
#[must_use]
pub fn parse_entries(body: &str) -> Vec<(i64, bool)> {
    let section = dependencies_section(body);
    item_re()
        .captures_iter(&section)
        .filter_map(|c| {
            let checked = c.get(1)?.as_str() != " ";
            Some((c.get(2)?.as_str().parse().ok()?, checked))
        })
        .collect()
}

/// One `<ref#>:<state-or-checked>` line per dependency, sorted
/// lexicographically (the shell's trailing `| sort`).
///
/// A checked item always renders `checked`, whatever `state` it carries — that
/// field is never consulted for one.
#[must_use]
pub fn deps_lines(deps: &[Dep]) -> String {
    let mut lines: Vec<String> = deps
        .iter()
        .map(|d| {
            if d.checked {
                format!("{}:checked", d.number)
            } else {
                format!("{}:{}", d.number, d.state.as_deref().unwrap_or("null"))
            }
        })
        .collect();
    lines.sort();
    lines.join("\n")
}

/// `blocked` iff any **unchecked** dependency is still OPEN.
///
/// `MERGED` and `CLOSED`-without-merging both count as resolved, matching
/// `curator.md`'s "When Dependencies Complete". Labels are never consulted, so
/// a referenced PR's review-cycle churn cannot flip this on its own.
#[must_use]
pub fn verdict(deps: &[Dep]) -> &'static str {
    if deps
        .iter()
        .any(|d| !d.checked && d.state.as_deref() == Some("OPEN"))
    {
        "blocked"
    } else {
        "clear"
    }
}

/// Compute the fingerprint.
#[must_use]
pub fn compute(deps: &[Dep]) -> Outcome {
    let lines = deps_lines(deps);
    let verdict = verdict(deps);
    // `printf '%s\n%s'` — no trailing newline.
    let hash = short_sha16(&format!("{verdict}\n{lines}"));
    Outcome {
        verdict: verdict.to_string(),
        deps: lines,
        conclusion_hash: hash,
    }
}

#[cfg(test)]
mod tests;
