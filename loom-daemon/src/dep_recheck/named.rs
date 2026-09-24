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
    /// `Some("owner/name")` for a cross-repo reference written `owner/repo#N`
    /// (#8502); `None` for a bare `#N`, which means the invoking repo.
    ///
    /// Load-bearing in two places: the live lookup must resolve the state in
    /// **that** repo rather than the invoking one, and the rendered
    /// [`deps_lines`] entry must name it, so `owner/a#5` and `owner/b#5` are
    /// two dependencies rather than one.
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub checked: bool,
    /// The referenced issue's or PR's own state. `None` for a checked item —
    /// never looked up, never consulted. Also `None` on a freshly parsed
    /// entry, before [`super::forge`] resolves it.
    #[serde(default)]
    pub state: Option<String>,
}

impl Dep {
    /// How this entry reads in prose and in error messages: `#123` for a
    /// same-repo reference, `owner/repo#123` for a cross-repo one.
    #[must_use]
    pub fn reference(&self) -> String {
        match self.repo.as_deref() {
            Some(r) => format!("{r}#{}", self.number),
            None => format!("#{}", self.number),
        }
    }

    /// The `deps_lines` key. A same-repo entry keeps the bare number the
    /// fingerprint has always used — changing it would move `CONCLUSION_HASH`
    /// for every existing same-repo dependency, manufacturing exactly the hash
    /// churn this subcommand exists to stop. A cross-repo entry is qualified,
    /// because the number alone does not identify it.
    fn line_key(&self) -> String {
        match self.repo.as_deref() {
            Some(r) => format!("{r}#{}", self.number),
            None => self.number.to_string(),
        }
    }
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
/// dependency phrase (`Blocked by`, `Depends on`, `Requires`, `**Epic**`),
/// an optional `PR `/`Issue ` token, and an optional `owner/repo` prefix
/// before the `#N`, in that order. All of the optional parts are
/// case-insensitive, and each may appear without the others
/// (`- [ ] Blocked by PR owner/repo#3` matches all three).
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
/// The optional `owner/repo` prefix is #8502, and is the same argument a third
/// time: a Curator in a *consumer* repo naturally writes an upstream
/// prerequisite as `- [ ] rjwalters/loom#8257: …`, because a bare `#8257`
/// would not resolve there. Before the prefix was accepted, such a line
/// matched **nothing** — the character after the checkbox is `r`, which is
/// neither a phrase, a `pr `/`issue ` token, nor a `#` — so the item was not
/// merely mis-parsed but invisible, and the subcommand reported
/// `DEPS=''`/`VERDICT=clear` for an issue whose upstream dependency was still
/// open. Live repro: `2AMLogic/2am#532`.
///
/// The prefix is deliberately charset-restricted (`owner` must start
/// alphanumeric; no spaces, quotes, or leading `-`) because it is fed to
/// `gh --repo`, and every issue body is untrusted input — see
/// `defaults/docs/untrusted-external-content.md`. A restricted charset that
/// cannot begin with `-` cannot be smuggled through as a flag.
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
        // then an optional `PR `/`Issue ` token (#7501), then an optional
        // `owner/repo` prefix (#8502), then the reference.
        // Kept on one line: the phrases themselves contain literal spaces, so
        // `(?x)` free-spacing mode would silently mangle the shared vocabulary.
        let pattern = format!(
            r"(?im)^[ \t]*[-*][ \t]*\[(?P<box>[ xX])\][ \t]*(?:(?:{DEPENDENCY_PHRASES})[*_: \t]*)?(?:(?:pr|issue)[ \t]+)?(?P<repo>{OWNER_REPO})?#(?P<num>[0-9]+)"
        );
        Regex::new(&pattern).expect("static checklist pattern")
    })
}

/// `owner/name`, as GitHub (and Gitea) spell it: an owner of alphanumerics and
/// hyphens that cannot start with a hyphen, then a repository name that may
/// also contain `.` and `_`.
///
/// Narrow on purpose — this value is passed to `gh --repo` verbatim, so it
/// must not be able to carry whitespace, a quote, or a leading `-`.
const OWNER_REPO: &str = r"[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9._-]+";

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

/// The checklist entries declared in `body`'s `## Dependencies` section, in
/// document order.
///
/// Each entry's `state` is `None` — parsing reads what the checklist *says*,
/// never the forge. [`super::forge::fetch_named_deps`] resolves the state of
/// every unchecked one afterwards, in the repo named by its own `repo` field.
#[must_use]
pub fn parse_entries(body: &str) -> Vec<Dep> {
    let section = dependencies_section(body);
    item_re()
        .captures_iter(&section)
        .filter_map(|c| {
            Some(Dep {
                number: c.name("num")?.as_str().parse().ok()?,
                repo: c.name("repo").map(|m| m.as_str().to_string()),
                checked: c.name("box")?.as_str() != " ",
                state: None,
            })
        })
        .collect()
}

/// One `<ref>:<state-or-checked>` line per dependency, sorted
/// lexicographically (the shell's trailing `| sort`).
///
/// `<ref>` is the bare number for a same-repo dependency (unchanged, so no
/// existing `CONCLUSION_HASH` moves) and `owner/repo#N` for a cross-repo one
/// (#8502) — see [`Dep::line_key`].
///
/// A checked item always renders `checked`, whatever `state` it carries — that
/// field is never consulted for one.
#[must_use]
pub fn deps_lines(deps: &[Dep]) -> String {
    let mut lines: Vec<String> = deps
        .iter()
        .map(|d| {
            if d.checked {
                format!("{}:checked", d.line_key())
            } else {
                format!("{}:{}", d.line_key(), d.state.as_deref().unwrap_or("null"))
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
