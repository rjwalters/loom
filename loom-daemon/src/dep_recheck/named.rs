//! `named-dependency` — the `## Dependencies` checklist fingerprint (#7314)
//! (epic #7810, PR 4).
//!
//! Covers the shape `dep-recheck` cannot see: a checklist item naming a
//! *different*, non-closing issue or PR as a prerequisite. #6335 was blocked on
//! #6333, which carries no `Closes #6335`, so it never appeared in
//! `closedByPullRequestsReferences` at all.

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

/// A checklist item: `- [ ] #123: ...`, with an optional case-insensitive
/// `PR `/`Issue ` token before the `#N`.
///
/// The optional token is #7501: curator prose naturally varies ("PR #N",
/// "Issue #N"), and silently dropping such an item produces a false
/// `VERDICT=clear` — which can unblock a Builder that is genuinely blocked.
/// Being slightly too permissive is the better failure direction here.
fn item_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?im)^[ \t]*[-*][ \t]*\[([ xX])\][ \t]*(?:(?:pr|issue)[ \t]+)?#([0-9]+)")
            .expect("static checklist pattern")
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
